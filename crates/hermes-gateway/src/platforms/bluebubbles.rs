//! BlueBubbles iMessage platform adapter.
//!
//! Connects to a local BlueBubbles macOS server for outbound REST sends and
//! inbound webhooks. Supports text messaging, media attachments (images,
//! voice, video, documents), tapback reactions, typing indicators, and
//! read receipts.
//!
//! Mirrors Python `gateway/platforms/bluebubbles.py`.

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as TokioMutex, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::platforms::helpers::{redact_phone, strip_markdown, MessageDeduplicator};
use crate::runner::MessageHandler;
use crate::session::{SessionSource, SessionStore};

// ── Constants ──────────────────────────────────────────────────────────────

const DEFAULT_WEBHOOK_HOST: &str = "127.0.0.1";
const DEFAULT_WEBHOOK_PORT: u16 = 8645;
const DEFAULT_WEBHOOK_PATH: &str = "/bluebubbles-webhook";
const MAX_TEXT_LENGTH: usize = 4000;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const ATTACHMENT_TIMEOUT_SECS: u64 = 120;

// Tapback reaction codes (BlueBubbles associatedMessageType values)
const TAPBACK_ADDED: &[(i64, &str)] = &[
    (2000, "love"),
    (2001, "like"),
    (2002, "dislike"),
    (2003, "laugh"),
    (2004, "emphasize"),
    (2005, "question"),
];
const TAPBACK_REMOVED: &[(i64, &str)] = &[
    (3000, "love"),
    (3001, "like"),
    (3002, "dislike"),
    (3003, "laugh"),
    (3004, "emphasize"),
    (3005, "question"),
];

// Webhook event types that carry user messages
const MESSAGE_EVENTS: &[&str] = &["new-message", "message", "updated-message"];

// ── Configuration ──────────────────────────────────────────────────────────

/// BlueBubbles platform configuration.
#[derive(Debug, Clone)]
pub struct BlueBubblesConfig {
    pub server_url: String,
    pub password: String,
    pub api_key: String,
    pub guid: String,
    pub webhook_host: String,
    pub webhook_port: u16,
    pub webhook_path: String,
    pub send_read_receipts: bool,
}

impl Default for BlueBubblesConfig {
    fn default() -> Self {
        Self {
            server_url: normalize_server_url(
                std::env::var("BLUEBUBBLES_SERVER_URL").unwrap_or_default(),
            ),
            password: std::env::var("BLUEBUBBLES_PASSWORD").unwrap_or_default(),
            api_key: std::env::var("BLUEBUBBLES_API_KEY").unwrap_or_default(),
            guid: std::env::var("BLUEBUBBLES_GUID").unwrap_or_default(),
            webhook_host: std::env::var("BLUEBUBBLES_WEBHOOK_HOST")
                .unwrap_or_else(|_| DEFAULT_WEBHOOK_HOST.to_string()),
            webhook_port: std::env::var("BLUEBUBBLES_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_WEBHOOK_PORT),
            webhook_path: {
                let path = std::env::var("BLUEBUBBLES_WEBHOOK_PATH")
                    .unwrap_or_else(|_| DEFAULT_WEBHOOK_PATH.to_string());
                if path.starts_with('/') { path } else { format!("/{path}") }
            },
            send_read_receipts: std::env::var("BLUEBUBBLES_SEND_READ_RECEIPTS")
                .ok()
                .map(|s| matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(true),
        }
    }
}

impl BlueBubblesConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    pub fn is_configured(&self) -> bool {
        !self.server_url.is_empty() && !self.password.is_empty()
    }
}

fn normalize_server_url(raw: String) -> String {
    let value = raw.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut value = value.to_string();
    if !value.starts_with("http://") && !value.starts_with("https://") {
        value = format!("http://{value}");
    }
    value.trim_end_matches('/').to_string()
}

// ── Inbound webhook query params ───────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookQuery {
    pub password: Option<String>,
    pub guid: Option<String>,
}

// ── Adapter ────────────────────────────────────────────────────────────────

/// BlueBubbles iMessage gateway adapter.
pub struct BlueBubblesAdapter {
    config: BlueBubblesConfig,
    client: Client,
    dedup: MessageDeduplicator,
    private_api_enabled: Arc<Mutex<Option<bool>>>,
    helper_connected: Arc<Mutex<bool>>,
    guid_cache: Arc<Mutex<HashMap<String, String>>>,
}

impl BlueBubblesAdapter {
    pub fn new(config: BlueBubblesConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| Client::new()),
            dedup: MessageDeduplicator::new(2000, 300.0),
            private_api_enabled: Arc::new(Mutex::new(None)),
            helper_connected: Arc::new(Mutex::new(false)),
            guid_cache: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    // ------------------------------------------------------------------
    // API helpers
    // ------------------------------------------------------------------

    fn api_url(&self, path: &str) -> String {
        let sep = if path.contains('?') { "&" } else { "?" };
        format!("{}{path}{sep}password={}", self.config.server_url,
            urlencoding::encode(&self.config.password))
    }

    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        let res = self
            .client
            .get(&self.api_url(path))
            .send()
            .await
            .map_err(|e| format!("BlueBubbles GET {path} failed: {e}"))?;
        let status = res.status();
        let body: serde_json::Value = res
            .json()
            .await
            .map_err(|e| format!("BlueBubbles GET {path} parse failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("BlueBubbles GET {path} returned {status}"));
        }
        Ok(body)
    }

    async fn api_post(
        &self,
        path: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let res = self
            .client
            .post(&self.api_url(path))
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("BlueBubbles POST {path} failed: {e}"))?;
        let status = res.status();
        let body: serde_json::Value = res
            .json()
            .await
            .map_err(|e| format!("BlueBubbles POST {path} parse failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("BlueBubbles POST {path} returned {status}"));
        }
        Ok(body)
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    async fn connect(&self) -> Result<(), String> {
        if self.config.server_url.is_empty() || self.config.password.is_empty() {
            return Err("BLUEBUBBLES_SERVER_URL and BLUEBUBBLES_PASSWORD are required".into());
        }
        self.api_get("/api/v1/ping").await?;
        let info = self.api_get("/api/v1/server/info").await?;
        let server_data = info.get("data").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let private_api = server_data.get("private_api").and_then(|v| v.as_bool()).unwrap_or(false);
        let helper = server_data.get("helper_connected").and_then(|v| v.as_bool()).unwrap_or(false);
        *self.private_api_enabled.lock().unwrap() = Some(private_api);
        *self.helper_connected.lock().unwrap() = helper;
        info!(
            "[bluebubbles] connected to {} (private_api={private_api}, helper={helper})",
            self.config.server_url,
        );
        Ok(())
    }

    fn webhook_url(&self) -> String {
        let host = if self.config.webhook_host == "0.0.0.0"
            || self.config.webhook_host == "127.0.0.1"
            || self.config.webhook_host == "localhost"
            || self.config.webhook_host == "::"
        {
            "localhost"
        } else {
            &self.config.webhook_host
        };
        format!("http://{host}:{}{}", self.config.webhook_port, self.config.webhook_path)
    }

    fn webhook_register_url(&self) -> String {
        let base = self.webhook_url();
        if self.config.password.is_empty() {
            base
        } else {
            format!("{}?password={}", base, urlencoding::encode(&self.config.password))
        }
    }

    async fn find_registered_webhooks(&self, url: &str) -> Result<Vec<serde_json::Value>, String> {
        let res = self.api_get("/api/v1/webhook").await?;
        let data = res.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        Ok(data.into_iter().filter(|wh| {
            wh.get("url").and_then(|v| v.as_str()) == Some(url)
        }).collect())
    }

    async fn register_webhook(&self) -> Result<bool, String> {
        let webhook_url = self.webhook_register_url();
        let existing = self.find_registered_webhooks(&webhook_url).await.unwrap_or_default();
        if !existing.is_empty() {
            info!("[bluebubbles] webhook already registered: {webhook_url}");
            return Ok(true);
        }
        let payload = serde_json::json!({
            "url": webhook_url,
            "events": ["new-message", "updated-message"],
        });
        match self.api_post("/api/v1/webhook", &payload).await {
            Ok(res) => {
                let status = res.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
                if (200..300).contains(&status) {
                    info!("[bluebubbles] webhook registered with server: {webhook_url}");
                    Ok(true)
                } else {
                    warn!(
                        "[bluebubbles] webhook registration returned status {status}: {}",
                        res.get("message").and_then(|v| v.as_str()).unwrap_or("unknown")
                    );
                    Ok(false)
                }
            }
            Err(e) => {
                warn!("[bluebubbles] failed to register webhook with server: {e}");
                Ok(false)
            }
        }
    }

    async fn unregister_webhook(&self) {
        let webhook_url = self.webhook_register_url();
        let Ok(existing) = self.find_registered_webhooks(&webhook_url).await else { return };
        let mut removed = false;
        for wh in existing {
            if let Some(wh_id) = wh.get("id").and_then(|v| v.as_str()) {
                if let Err(e) = self
                    .client
                    .delete(&self.api_url(&format!("/api/v1/webhook/{wh_id}")))
                    .send()
                    .await
                {
                    debug!("[bluebubbles] failed to delete webhook {wh_id}: {e}");
                } else {
                    removed = true;
                }
            }
        }
        if removed {
            info!("[bluebubbles] webhook unregistered: {webhook_url}");
        }
    }

    // ------------------------------------------------------------------
    // Chat GUID resolution
    // ------------------------------------------------------------------

    async fn resolve_chat_guid(&self, target: &str) -> Option<String> {
        let target = target.trim();
        if target.is_empty() {
            return None;
        }
        if target.contains(';') {
            return Some(target.to_string());
        }
        {
            let cache = self.guid_cache.lock().unwrap();
            if let Some(guid) = cache.get(target) {
                return Some(guid.clone());
            }
        }
        let payload = serde_json::json!({
            "limit": 100,
            "offset": 0,
            "with": ["participants"],
        });
        let Ok(res) = self.api_post("/api/v1/chat/query", &payload).await else {
            return None;
        };
        let chats = res.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for chat in chats {
            let guid = chat.get("guid").or_else(|| chat.get("chatGuid"))
                .and_then(|v| v.as_str());
            let identifier = chat.get("chatIdentifier").or_else(|| chat.get("identifier"))
                .and_then(|v| v.as_str());
            if identifier == Some(target) {
                if let Some(g) = guid {
                    self.guid_cache.lock().unwrap().insert(target.to_string(), g.to_string());
                    return Some(g.to_string());
                }
            }
            if let Some(parts) = chat.get("participants").and_then(|v| v.as_array()) {
                for part in parts {
                    let addr = part.get("address").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if addr == target {
                        if let Some(g) = guid {
                            self.guid_cache.lock().unwrap().insert(target.to_string(), g.to_string());
                            return Some(g.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    async fn create_chat_for_handle(&self, address: &str, message: &str) -> Result<String, String> {
        let payload = serde_json::json!({
            "addresses": [address],
            "message": message,
            "tempGuid": format!("temp-{}", now_secs()),
        });
        let res = self.api_post("/api/v1/chat/new", &payload).await?;
        let data = res.get("data").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let msg_id = data.get("guid").or_else(|| data.get("messageGuid"))
            .and_then(|v| v.as_str()).unwrap_or("ok");
        Ok(msg_id.to_string())
    }

    // ------------------------------------------------------------------
    // Text sending
    // ------------------------------------------------------------------

    pub async fn send_text(&self, chat_id: &str, content: &str) -> Result<String, String> {
        let text = strip_markdown(content);
        if text.is_empty() {
            return Err("BlueBubbles send requires text".into());
        }
        let chunks = chunk_text(&text, MAX_TEXT_LENGTH);
        let mut last_id = String::new();
        for chunk in chunks {
            let guid = match self.resolve_chat_guid(chat_id).await {
                Some(g) => g,
                None => {
                    let private_api = self.private_api_enabled.lock().unwrap().unwrap_or(false);
                    if private_api && (chat_id.contains('@') || chat_id.starts_with('+')) {
                        return self.create_chat_for_handle(chat_id, &chunk).await;
                    }
                    return Err(format!("BlueBubbles chat not found for target: {chat_id}"));
                }
            };
            let payload = serde_json::json!({
                "chatGuid": guid,
                "tempGuid": format!("temp-{}", now_secs()),
                "message": chunk,
            });
            let res = self.api_post("/api/v1/message/text", &payload).await?;
            let data = res.get("data").and_then(|v| v.as_object()).cloned().unwrap_or_default();
            let msg_id = data.get("guid").or_else(|| data.get("messageGuid"))
                .and_then(|v| v.as_str()).unwrap_or("ok");
            last_id = msg_id.to_string();
        }
        Ok(last_id)
    }

    // ------------------------------------------------------------------
    // Media sending (outbound)
    // ------------------------------------------------------------------

    pub async fn send_attachment(
        &self,
        chat_id: &str,
        file_path: &str,
        filename: Option<&str>,
        caption: Option<&str>,
        is_audio_message: bool,
    ) -> Result<String, String> {
        let guid = self.resolve_chat_guid(chat_id).await
            .ok_or_else(|| format!("Chat not found: {chat_id}"))?;
        let fname = filename.unwrap_or_else(|| {
            Path::new(file_path).file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
        });
        let file_data = tokio::fs::read(file_path).await
            .map_err(|e| format!("Failed to read attachment {file_path}: {e}"))?;

        let form = reqwest::multipart::Form::new()
            .text("chatGuid", guid)
            .text("name", fname.to_string())
            .text("tempGuid", uuid::Uuid::new_v4().to_string())
            .part("attachment", reqwest::multipart::Part::bytes(file_data)
                .file_name(fname.to_string())
                .mime_str("application/octet-stream")
                .map_err(|e| e.to_string())?);
        let form = if is_audio_message {
            form.text("isAudioMessage", "true")
        } else {
            form
        };

        let res = self
            .client
            .post(&self.api_url("/api/v1/message/attachment"))
            .multipart(form)
            .timeout(std::time::Duration::from_secs(ATTACHMENT_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| format!("BlueBubbles attachment upload failed: {e}"))?;

        let status = res.status();
        let body: serde_json::Value = res.json().await
            .map_err(|e| format!("BlueBubbles attachment response parse failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("BlueBubbles attachment upload returned {status}"));
        }

        let data = body.get("data").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let msg_id = data.get("guid").and_then(|v| v.as_str()).unwrap_or("ok").to_string();

        if let Some(cap) = caption {
            let _ = self.send_text(chat_id, cap).await;
        }

        Ok(msg_id)
    }

    pub async fn send_image(
        &self,
        chat_id: &str,
        image_path: &str,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_attachment(chat_id, image_path, None, caption, false).await
    }

    pub async fn send_voice(
        &self,
        chat_id: &str,
        audio_path: &str,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_attachment(chat_id, audio_path, None, caption, true).await
    }

    pub async fn send_video(
        &self,
        chat_id: &str,
        video_path: &str,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_attachment(chat_id, video_path, None, caption, false).await
    }

    pub async fn send_document(
        &self,
        chat_id: &str,
        file_path: &str,
        file_name: Option<&str>,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_attachment(chat_id, file_path, file_name, caption, false).await
    }

    // ------------------------------------------------------------------
    // Typing indicators
    // ------------------------------------------------------------------

    pub async fn send_typing(&self, chat_id: &str) {
        if !self.has_private_api() {
            return;
        }
        if let Some(guid) = self.resolve_chat_guid(chat_id).await {
            let encoded = urlencoding::encode(&guid);
            let _ = self
                .client
                .post(&self.api_url(&format!("/api/v1/chat/{encoded}/typing")))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        }
    }

    pub async fn stop_typing(&self, chat_id: &str) {
        if !self.has_private_api() {
            return;
        }
        if let Some(guid) = self.resolve_chat_guid(chat_id).await {
            let encoded = urlencoding::encode(&guid);
            let _ = self
                .client
                .delete(&self.api_url(&format!("/api/v1/chat/{encoded}/typing")))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        }
    }

    // ------------------------------------------------------------------
    // Read receipts
    // ------------------------------------------------------------------

    pub async fn mark_read(&self, chat_id: &str) -> bool {
        if !self.has_private_api() {
            return false;
        }
        if let Some(guid) = self.resolve_chat_guid(chat_id).await {
            let encoded = urlencoding::encode(&guid);
            match self
                .client
                .post(&self.api_url(&format!("/api/v1/chat/{encoded}/read")))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
            {
                Ok(res) => res.status().is_success(),
                Err(_) => false,
            }
        } else {
            false
        }
    }

    // ------------------------------------------------------------------
    // Chat info
    // ------------------------------------------------------------------

    pub async fn get_chat_info(&self, chat_id: &str) -> HashMap<String, serde_json::Value> {
        let is_group = chat_id.contains(";+;");
        let mut info: HashMap<String, serde_json::Value> = HashMap::new();
        info.insert("name".to_string(), serde_json::Value::String(chat_id.to_string()));
        info.insert("type".to_string(), serde_json::Value::String(if is_group { "group".into() } else { "dm".into() }));

        if let Some(guid) = self.resolve_chat_guid(chat_id).await {
            let encoded = urlencoding::encode(&guid);
            if let Ok(res) = self.api_get(&format!("/api/v1/chat/{encoded}?with=participants")).await {
                let data = res.get("data").and_then(|v| v.as_object()).cloned().unwrap_or_default();
                let display_name = data.get("displayName")
                    .or_else(|| data.get("chatIdentifier"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(chat_id);
                info.insert("name".to_string(), serde_json::Value::String(display_name.to_string()));
                let participants: Vec<String> = data.get("participants")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter()
                        .filter_map(|p| p.get("address").and_then(|v| v.as_str()).map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect())
                    .unwrap_or_default();
                if !participants.is_empty() {
                    info.insert("participants".to_string(), serde_json::Value::Array(
                        participants.into_iter().map(serde_json::Value::String).collect()
                    ));
                }
            }
        }
        info
    }

    pub fn format_message(&self, content: &str) -> String {
        strip_markdown(content)
    }

    // ------------------------------------------------------------------
    // Inbound attachment downloading
    // ------------------------------------------------------------------

    async fn download_attachment(
        &self,
        att_guid: &str,
        att_meta: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<String> {
        let encoded = urlencoding::encode(att_guid);
        let resp = match self
            .client
            .get(&self.api_url(&format!("/api/v1/attachment/{encoded}/download")))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("[bluebubbles] failed to download attachment {}: {e}", redact_phone(att_guid));
                return None;
            }
        };
        let data = match resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                warn!("[bluebubbles] failed to read attachment bytes {}: {e}", redact_phone(att_guid));
                return None;
            }
        };

        let mime = att_meta.get("mimeType").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        let transfer_name = att_meta.get("transferName").and_then(|v| v.as_str()).unwrap_or("");

        if mime.starts_with("image/") {
            let ext = match mime.as_str() {
                "image/jpeg" | "image/heic" | "image/heif" | "image/tiff" => ".jpg",
                "image/png" => ".png",
                "image/gif" => ".gif",
                "image/webp" => ".webp",
                _ => ".jpg",
            };
            return cache_image_from_bytes(&data, ext).ok();
        }

        if mime.starts_with("audio/") {
            let ext = match mime.as_str() {
                "audio/mp3" | "audio/mpeg" => ".mp3",
                "audio/ogg" => ".ogg",
                "audio/wav" => ".wav",
                "audio/x-caf" => ".mp3",
                "audio/mp4" | "audio/aac" | "audio/m4a" => ".m4a",
                _ => ".mp3",
            };
            return cache_audio_from_bytes(&data, ext).ok();
        }

        let filename = if transfer_name.is_empty() {
            format!("file_{}", &uuid::Uuid::new_v4().to_string()[..8])
        } else {
            transfer_name.to_string()
        };
        cache_document_from_bytes(&data, &filename).ok()
    }

    // ------------------------------------------------------------------
    // Webhook server
    // ------------------------------------------------------------------

    pub async fn run(
        &self,
        handler: Arc<TokioMutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: Arc<SessionStore>,
        shutdown_rx: oneshot::Receiver<()>,
        default_model: String,
        per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
    ) -> Result<(), String> {
        if let Err(e) = self.connect().await {
            return Err(format!("[bluebubbles] connect failed: {e}"));
        }

        if let Ok(true) = self.register_webhook().await {
            // registered
        }

        let state = WebhookState {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            private_api_enabled: self.private_api_enabled.clone(),
            helper_connected: self.helper_connected.clone(),
            guid_cache: self.guid_cache.clone(),
            handler,
            running,
            running_sessions,
            busy_ack_ts,
            session_store,
            default_model,
            per_chat_model,
        };

        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route(self.config.webhook_path.as_str(), post(handle_webhook))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind((
            self.config.webhook_host.as_str(),
            self.config.webhook_port,
        ))
        .await
        .map_err(|e| {
            format!(
                "Failed to bind BlueBubbles webhook server on {}:{}: {e}",
                self.config.webhook_host, self.config.webhook_port
            )
        })?;

        info!(
            "[bluebubbles] webhook server listening on {}:{}{}",
            self.config.webhook_host,
            self.config.webhook_port,
            self.config.webhook_path,
        );

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("[bluebubbles] Shutting down webhook server");
            })
            .await
            .map_err(|e| format!("BlueBubbles webhook server error: {e}"))
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn has_private_api(&self) -> bool {
        let private_api = self.private_api_enabled.lock().unwrap().unwrap_or(false);
        let helper = *self.helper_connected.lock().unwrap();
        private_api && helper
    }
}

// Cloneable state for axum handlers
#[derive(Clone)]
struct WebhookState {
    config: BlueBubblesConfig,
    client: Client,
    dedup: MessageDeduplicator,
    private_api_enabled: Arc<Mutex<Option<bool>>>,
    helper_connected: Arc<Mutex<bool>>,
    guid_cache: Arc<Mutex<HashMap<String, String>>>,
    handler: Arc<TokioMutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
}

impl WebhookState {
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let adapter = BlueBubblesAdapter {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            private_api_enabled: self.private_api_enabled.clone(),
            helper_connected: self.helper_connected.clone(),
            guid_cache: self.guid_cache.clone(),
        };
        adapter.send_text(chat_id, text).await
    }

    async fn mark_read(&self, chat_id: &str) -> bool {
        let adapter = BlueBubblesAdapter {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            private_api_enabled: self.private_api_enabled.clone(),
            helper_connected: self.helper_connected.clone(),
            guid_cache: self.guid_cache.clone(),
        };
        adapter.mark_read(chat_id).await
    }

    fn has_private_api(&self) -> bool {
        let private_api = self.private_api_enabled.lock().unwrap().unwrap_or(false);
        let helper = *self.helper_connected.lock().unwrap();
        private_api && helper
    }

    async fn resolve_chat_guid(&self, target: &str) -> Option<String> {
        let adapter = BlueBubblesAdapter {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            private_api_enabled: self.private_api_enabled.clone(),
            helper_connected: self.helper_connected.clone(),
            guid_cache: self.guid_cache.clone(),
        };
        adapter.resolve_chat_guid(target).await
    }

    async fn download_attachment(
        &self,
        att_guid: &str,
        att_meta: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<String> {
        let adapter = BlueBubblesAdapter {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            private_api_enabled: self.private_api_enabled.clone(),
            helper_connected: self.helper_connected.clone(),
            guid_cache: self.guid_cache.clone(),
        };
        adapter.download_attachment(att_guid, att_meta).await
    }
}

// ------------------------------------------------------------------
// HTTP handlers
// ------------------------------------------------------------------

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<WebhookQuery>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Auth
    let token = query
        .password
        .or(query.guid)
        .or_else(|| headers.get("x-password").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .or_else(|| headers.get("x-guid").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .or_else(|| headers.get("x-bluebubbles-guid").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .unwrap_or_default();
    if token != state.config.password {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": "unauthorized"})));
    }

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Try form-encoded fallback
            let body_str = String::from_utf8_lossy(&body);
            let form: HashMap<String, String> = url::form_urlencoded::parse(body_str.as_bytes())
                .into_owned()
                .collect();
            let payload_str = form.get("payload").or(form.get("data")).or(form.get("message")).cloned().unwrap_or_default();
            match serde_json::from_str(&payload_str) {
                Ok(v) => v,
                Err(_) => {
                    error!("[bluebubbles] webhook parse error: invalid payload");
                    return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid payload"})));
                }
            }
        }
    };

    let event_type = value_str(payload.get("type")).or_else(|| value_str(payload.get("event"))).unwrap_or_default();
    if !event_type.is_empty() && !MESSAGE_EVENTS.contains(&event_type.as_str()) {
        return (StatusCode::OK, axum::Json(serde_json::json!({"status": "ok"})));
    }

    let record = extract_payload_record(&payload);
    let record_obj = record.as_object().cloned().unwrap_or_default();

    let is_from_me = record_obj.get("isFromMe").or(record_obj.get("fromMe")).or(record_obj.get("is_from_me"))
        .and_then(|v| v.as_bool()).unwrap_or(false);
    if is_from_me {
        return (StatusCode::OK, axum::Json(serde_json::json!({"status": "ok"})));
    }

    // Skip tapback reactions
    if let Some(assoc_type) = record_obj.get("associatedMessageType").and_then(|v| v.as_i64()) {
        if TAPBACK_ADDED.iter().any(|(t, _)| *t == assoc_type) || TAPBACK_REMOVED.iter().any(|(t, _)| *t == assoc_type) {
            return (StatusCode::OK, axum::Json(serde_json::json!({"status": "ok"})));
        }
    }

    let mut text = value_str(record_obj.get("text"))
        .or_else(|| value_str(record_obj.get("message")))
        .or_else(|| value_str(record_obj.get("body")))
        .unwrap_or_default();

    // Inbound attachment handling
    let attachments = record_obj.get("attachments").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut media_urls: Vec<String> = Vec::new();
    let mut media_types: Vec<String> = Vec::new();
    for att in attachments {
        let att_obj = match att.as_object() {
            Some(o) => o,
            None => continue,
        };
        let att_guid = att_obj.get("guid").and_then(|v| v.as_str()).unwrap_or("");
        if att_guid.is_empty() {
            continue;
        }
        if let Some(cached) = state.download_attachment(att_guid, att_obj).await {
            let mime = att_obj.get("mimeType").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            media_urls.push(cached);
            media_types.push(mime.clone());
            // msg_type tracked for parity with Python; not needed in Rust handler
        }
    }

    if text.is_empty() && !media_urls.is_empty() {
        text = "(attachment)".to_string();
    }

    let chat_guid = value_str(record_obj.get("chatGuid"))
        .or_else(|| value_str(payload.get("chatGuid")))
        .or_else(|| value_str(record_obj.get("chat_guid")))
        .or_else(|| value_str(payload.get("chat_guid")))
        .or_else(|| value_str(payload.get("guid")));

    let mut chat_guid = chat_guid.unwrap_or_default();
    if chat_guid.is_empty() {
        if let Some(chats) = record_obj.get("chats").and_then(|v| v.as_array()) {
            if let Some(first) = chats.first().and_then(|v| v.as_object()) {
                chat_guid = value_str(first.get("guid"))
                    .or_else(|| value_str(first.get("chatGuid")))
                    .unwrap_or_default();
            }
        }
    }

    let chat_identifier = value_str(record_obj.get("chatIdentifier"))
        .or_else(|| value_str(record_obj.get("identifier")))
        .or_else(|| value_str(payload.get("chatIdentifier")))
        .or_else(|| value_str(payload.get("identifier")));

    let sender = {
        let handle_addr = record_obj.get("handle")
            .and_then(|v| v.as_object())
            .and_then(|h| h.get("address"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        handle_addr
            .or_else(|| value_str(record_obj.get("sender")))
            .or_else(|| value_str(record_obj.get("from")))
            .or_else(|| value_str(record_obj.get("address")))
            .or_else(|| chat_identifier.clone())
            .unwrap_or_else(|| chat_guid.clone())
    };

    let mut chat_identifier = chat_identifier.unwrap_or_default();
    if chat_guid.is_empty() && chat_identifier.is_empty() && !sender.is_empty() {
        chat_identifier = sender.clone();
    }
    if sender.is_empty() || (chat_guid.is_empty() && chat_identifier.is_empty()) || text.is_empty() {
        return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "missing message fields"})));
    }

    let session_chat_id = if !chat_guid.is_empty() { chat_guid.clone() } else { chat_identifier.clone() };
    let is_group = record_obj.get("isGroup").and_then(|v| v.as_bool()).unwrap_or(false) || chat_guid.contains(";+;");

    let message_id = value_str(record_obj.get("guid"))
        .or_else(|| value_str(record_obj.get("messageGuid")))
        .or_else(|| value_str(record_obj.get("id")))
        .unwrap_or_default();

    if !message_id.is_empty() && state.dedup.is_duplicate(&message_id) {
        debug!("[bluebubbles] dedup: skipping {message_id}");
        return (StatusCode::OK, axum::Json(serde_json::json!({"status": "ok"})));
    }

    info!(
        "[bluebubbles] inbound from {} in {}: {}",
        redact_phone(&sender),
        redact_phone(&session_chat_id),
        &text[..text.len().min(80)],
    );

    let chat_id_for_handler = session_chat_id.clone();
    let content_for_handler = text.clone();
    let handler = state.handler.clone();
    let running = state.running.clone();
    let running_sessions = state.running_sessions.clone();
    let busy_ack_ts = state.busy_ack_ts.clone();
    let session_store = state.session_store.clone();
    let per_chat_model = state.per_chat_model.clone();
    let state_for_send = state.clone();

    tokio::spawn(async move {
        if !running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        // Busy session check
        let busy_elapsed_min: Option<f64> = {
            let sessions = running_sessions.lock();
            sessions.get(&chat_id_for_handler).map(|&start_ts| (now - start_ts) / 60.0)
        };

        if let Some(elapsed_min) = busy_elapsed_min {
            let should_ack = {
                let mut ack_map = busy_ack_ts.lock();
                let last_ack = ack_map.get(&chat_id_for_handler).copied().unwrap_or(0.0);
                if now - last_ack < 30.0 {
                    false
                } else {
                    ack_map.insert(chat_id_for_handler.clone(), now);
                    true
                }
            };
            if should_ack {
                let handler_guard = handler.lock().await;
                if let Some(h) = handler_guard.as_ref() {
                    h.interrupt(&chat_id_for_handler, &content_for_handler);
                }
                drop(handler_guard);
                info!("Session {chat_id_for_handler}: busy — agent interrupted after {elapsed_min:.1} min");
                let busy_msg = format!(
                    "Still processing your previous message ({:.0}m elapsed). \
                     Please wait for my response before sending another prompt.",
                    elapsed_min
                );
                let _ = state_for_send.send_text(&chat_id_for_handler, &busy_msg).await;
            }
            return;
        }

        let handler_guard = handler.lock().await;
        let Some(handler_ref) = handler_guard.as_ref().cloned() else {
            warn!("No message handler registered for BlueBubbles messages");
            return;
        };
        drop(handler_guard);

        {
            let mut sessions = running_sessions.lock();
            sessions.insert(chat_id_for_handler.clone(), now);
        }

        let model_override = per_chat_model.lock().get(&chat_id_for_handler).cloned();
        let source = SessionSource {
            platform: Platform::Bluebubbles,
            chat_id: chat_id_for_handler.clone(),
            chat_name: Some(if chat_identifier.is_empty() { sender.clone() } else { chat_identifier.clone() }),
            chat_type: if is_group { "group".to_string() } else { "dm".to_string() },
            user_id: Some(sender.clone()),
            user_name: Some(sender.clone()),
            thread_id: None,
            chat_topic: None,
            user_id_alt: None,
            chat_id_alt: if chat_identifier.is_empty() { None } else { Some(chat_identifier.clone()) },
        };

        let content = if media_urls.is_empty() {
            content_for_handler
        } else {
            format!("{}\n[attachments: {}]", content_for_handler, media_urls.join(", "))
        };

        match handler_ref.handle_message(Platform::Bluebubbles, &chat_id_for_handler, &content, model_override.as_deref()).await {
            Ok(result) => {
                running_sessions.lock().remove(&chat_id_for_handler);
                busy_ack_ts.lock().remove(&chat_id_for_handler);

                if result.compression_exhausted {
                    session_store.reset_session_for(&source);
                    let _ = state_for_send
                        .send_text(&chat_id_for_handler, "Session reset: conversation context grew too large. Starting fresh.")
                        .await;
                }
                if !result.response.is_empty() {
                    let _ = state_for_send.send_text(&chat_id_for_handler, &result.response).await;
                }
            }
            Err(e) => {
                running_sessions.lock().remove(&chat_id_for_handler);
                busy_ack_ts.lock().remove(&chat_id_for_handler);
                error!("Agent handler failed for BlueBubbles message: {e}");
                let _ = state_for_send
                    .send_text(&chat_id_for_handler, "Sorry, I encountered an error processing your message.")
                    .await;
            }
        }

        // Fire-and-forget read receipt
        if state_for_send.config.send_read_receipts {
            tokio::spawn(async move {
                let _ = state_for_send.mark_read(&chat_id_for_handler).await;
            });
        }
    });

    (StatusCode::OK, axum::Json(serde_json::json!({"status": "ok"})))
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

fn extract_payload_record(payload: &serde_json::Value) -> serde_json::Value {
    if let Some(data) = payload.get("data") {
        if data.is_object() {
            return data.clone();
        }
        if let Some(arr) = data.as_array() {
            for item in arr {
                if item.is_object() {
                    return item.clone();
                }
            }
        }
    }
    if let Some(msg) = payload.get("message") {
        if msg.is_object() {
            return msg.clone();
        }
    }
    if payload.is_object() {
        payload.clone()
    } else {
        serde_json::Value::Object(Default::default())
    }
}

fn value_str(value: Option<&serde_json::Value>) -> Option<String> {
    value.and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let split_at = if remaining.len() <= max_len {
            remaining.len()
        } else {
            let mut pos = max_len;
            while pos > 0 && !remaining.is_char_boundary(pos) {
                pos -= 1;
            }
            if pos == 0 {
                pos = max_len;
            }
            pos
        };
        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.to_string());
        remaining = rest;
    }
    chunks
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn cache_image_from_bytes(data: &[u8], ext: &str) -> Result<String, String> {
    let cache_dir = hermes_core::get_hermes_home().join("cache").join("images");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create image cache dir: {e}"))?;
    let name = format!("img_{}{ext}", uuid::Uuid::new_v4().simple());
    let path = cache_dir.join(&name);
    std::fs::write(&path, data).map_err(|e| format!("Failed to write image cache: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn cache_audio_from_bytes(data: &[u8], ext: &str) -> Result<String, String> {
    let cache_dir = hermes_core::get_hermes_home().join("cache").join("audio");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create audio cache dir: {e}"))?;
    let name = format!("audio_{}{ext}", uuid::Uuid::new_v4().simple());
    let path = cache_dir.join(&name);
    std::fs::write(&path, data).map_err(|e| format!("Failed to write audio cache: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn cache_document_from_bytes(data: &[u8], filename: &str) -> Result<String, String> {
    let safe_name = Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document")
        .replace('\x00', "")
        .trim()
        .to_string();
    if safe_name.is_empty() {
        return cache_image_from_bytes(data, ".bin");
    }
    let cache_dir = hermes_core::get_hermes_home().join("cache").join("documents");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create document cache dir: {e}"))?;
    let name = format!("doc_{}_{safe_name}", uuid::Uuid::new_v4().simple());
    let path = cache_dir.join(&name);
    std::fs::write(&path, data).map_err(|e| format!("Failed to write document cache: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}
