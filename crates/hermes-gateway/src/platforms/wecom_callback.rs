//! WeCom callback-mode platform adapter for self-built enterprise applications.
//!
//! Handles the standard WeCom callback flow: WeCom POSTs encrypted XML to an
//! HTTP endpoint, the adapter decrypts it, forwards the message to the agent,
//! and immediately acknowledges.  The agent's reply is delivered later via the
//! proactive `message/send` API using an access-token.
//!
//! Mirrors Python `gateway/platforms/wecom_callback.py`.

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::platforms::helpers::MessageDeduplicator;
use crate::runner::MessageHandler;
use crate::session::{SessionSource, SessionStore};

// ── Constants ──────────────────────────────────────────────────────────────

const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8645;
const DEFAULT_PATH: &str = "/wecom/callback";
const ACCESS_TOKEN_TTL_SECONDS: u64 = 7200;
const MESSAGE_DEDUP_TTL_SECONDS: u64 = 300;
const MAX_MESSAGE_LENGTH: usize = 2048;

// ── Configuration ──────────────────────────────────────────────────────────

/// WeCom callback platform configuration.
#[derive(Debug, Clone)]
pub struct WecomCallbackConfig {
    pub token: String,
    pub encoding_aes_key: String,
    pub corp_id: String,
    pub corp_secret: String,
    pub agent_id: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Default for WecomCallbackConfig {
    fn default() -> Self {
        Self {
            token: std::env::var("WECOM_CALLBACK_TOKEN").unwrap_or_default(),
            encoding_aes_key: std::env::var("WECOM_CALLBACK_ENCODING_AES_KEY").unwrap_or_default(),
            corp_id: std::env::var("WECOM_CALLBACK_CORP_ID").unwrap_or_default(),
            corp_secret: std::env::var("WECOM_CALLBACK_CORP_SECRET").unwrap_or_default(),
            agent_id: std::env::var("WECOM_CALLBACK_AGENT_ID").unwrap_or_default(),
            host: std::env::var("WECOM_CALLBACK_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string()),
            port: std::env::var("WECOM_CALLBACK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PORT),
            path: std::env::var("WECOM_CALLBACK_PATH")
                .ok()
                .unwrap_or_else(|| DEFAULT_PATH.to_string()),
        }
    }
}

impl WecomCallbackConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    pub fn is_configured(&self) -> bool {
        !self.token.is_empty()
            && !self.encoding_aes_key.is_empty()
            && !self.corp_id.is_empty()
            && !self.corp_secret.is_empty()
    }
}

// ── Query parameters ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct VerifyQuery {
    msg_signature: String,
    timestamp: String,
    nonce: String,
    echostr: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CallbackQuery {
    msg_signature: String,
    timestamp: String,
    nonce: String,
}

// ── Adapter ────────────────────────────────────────────────────────────────

/// WeCom callback-mode gateway adapter.
pub struct WecomCallbackAdapter {
    config: WecomCallbackConfig,
    client: Client,
    dedup: MessageDeduplicator,
    access_token: Arc<Mutex<Option<(String, f64)>>>,
}

impl WecomCallbackAdapter {
    pub fn new(config: WecomCallbackConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            dedup: MessageDeduplicator::new(2000, MESSAGE_DEDUP_TTL_SECONDS as f64),
            access_token: Arc::new(Mutex::new(None)),
            config,
        }
    }

    // ------------------------------------------------------------------
    // Access-token management
    // ------------------------------------------------------------------

    async fn get_access_token(&self) -> Result<String, String> {
        {
            let guard = self.access_token.lock().await;
            if let Some((token, expires_at)) = guard.as_ref() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                if *expires_at > now + 60.0 {
                    return Ok(token.clone());
                }
            }
        }
        self.refresh_access_token().await
    }

    async fn refresh_access_token(&self) -> Result<String, String> {
        let resp = self
            .client
            .get("https://qyapi.weixin.qq.com/cgi-bin/gettoken")
            .query(&[
                ("corpid", self.config.corp_id.as_str()),
                ("corpsecret", self.config.corp_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("WeCom token request failed: {e}"))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("WeCom token response parse failed: {e}"))?;

        let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "WeCom token refresh failed: errcode={errcode}, errmsg={}",
                data.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }

        let token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or("Missing access_token in response")?
            .to_string();
        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(ACCESS_TOKEN_TTL_SECONDS);
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            + expires_in as f64;

        {
            let mut guard = self.access_token.lock().await;
            *guard = Some((token.clone(), expires_at));
        }

        info!(
            "[wecom_callback] Token refreshed (corp={}), expires in {}s",
            self.config.corp_id, expires_in
        );
        Ok(token)
    }

    // ------------------------------------------------------------------
    // Send
    // ------------------------------------------------------------------

    /// Send a text message via WeCom `message/send` API.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let touser = chat_id.split(':').next_back().unwrap_or(chat_id);
        let agent_id: i64 = self.config.agent_id.parse().unwrap_or(0);

        let payload = serde_json::json!({
            "touser": touser,
            "msgtype": "text",
            "agentid": agent_id,
            "text": {
                "content": text.chars().take(MAX_MESSAGE_LENGTH).collect::<String>(),
            },
            "safe": 0,
        });

        let resp = self
            .client
            .post(format!(
                "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token={token}"
            ))
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("WeCom send request failed: {e}"))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("WeCom send response parse failed: {e}"))?;

        let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "WeCom send failed: errcode={errcode}, errmsg={}",
                data.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }

        Ok(data
            .get("msgid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    // ------------------------------------------------------------------
    // Webhook server
    // ------------------------------------------------------------------

    /// Run the WeCom callback HTTP server.
    pub async fn run(
        &self,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: Arc<SessionStore>,
        shutdown_rx: oneshot::Receiver<()>,
        default_model: String,
        per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
    ) -> Result<(), String> {
        let state = CallbackState {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            access_token: self.access_token.clone(),
            handler,
            running,
            running_sessions,
            busy_ack_ts,
            session_store,
            default_model,
            per_chat_model,
        };

        let app = Router::new()
            .route("/health", get(handle_health))
            .route(&self.config.path, get(handle_verify))
            .route(&self.config.path, post(handle_callback))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind((self.config.host.as_str(), self.config.port))
            .await
            .map_err(|e| {
                format!(
                    "Failed to bind WeCom callback server on {}:{}: {}",
                    self.config.host, self.config.port, e
                )
            })?;

        info!(
            "[wecom_callback] HTTP server listening on {}:{}{}",
            self.config.host, self.config.port, self.config.path
        );

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("[wecom_callback] Shutting down");
            })
            .await
            .map_err(|e| format!("WeCom callback server error: {e}"))
    }
}

// ── Shared axum state ──────────────────────────────────────────────────────

#[derive(Clone)]
struct CallbackState {
    config: WecomCallbackConfig,
    client: Client,
    dedup: MessageDeduplicator,
    access_token: Arc<Mutex<Option<(String, f64)>>>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
}

impl CallbackState {
    async fn get_access_token(&self) -> Result<String, String> {
        {
            let guard = self.access_token.lock().await;
            if let Some((token, expires_at)) = guard.as_ref() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                if *expires_at > now + 60.0 {
                    return Ok(token.clone());
                }
            }
        }
        let token = self._refresh_access_token().await?;
        let mut guard = self.access_token.lock().await;
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            + ACCESS_TOKEN_TTL_SECONDS as f64;
        *guard = Some((token.clone(), expires_at));
        Ok(token)
    }

    async fn _refresh_access_token(&self) -> Result<String, String> {
        let resp = self
            .client
            .get("https://qyapi.weixin.qq.com/cgi-bin/gettoken")
            .query(&[
                ("corpid", self.config.corp_id.as_str()),
                ("corpsecret", self.config.corp_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("WeCom token request failed: {e}"))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("WeCom token response parse failed: {e}"))?;

        let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "WeCom token refresh failed: errcode={errcode}, errmsg={}",
                data.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }

        data.get("access_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| "Missing access_token in response".to_string())
    }

    async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let touser = chat_id.split(':').next_back().unwrap_or(chat_id);
        let agent_id: i64 = self.config.agent_id.parse().unwrap_or(0);

        let payload = serde_json::json!({
            "touser": touser,
            "msgtype": "text",
            "agentid": agent_id,
            "text": {
                "content": text.chars().take(MAX_MESSAGE_LENGTH).collect::<String>(),
            },
            "safe": 0,
        });

        let resp = self
            .client
            .post(format!(
                "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token={token}"
            ))
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("WeCom send request failed: {e}"))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("WeCom send response parse failed: {e}"))?;

        let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "WeCom send failed: errcode={errcode}, errmsg={}",
                data.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }

        Ok(data
            .get("msgid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

// ── HTTP handlers ──────────────────────────────────────────────────────────

async fn handle_health() -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok", "platform": "wecom_callback"})),
    )
}

async fn handle_verify(
    State(state): State<CallbackState>,
    Query(query): Query<VerifyQuery>,
) -> impl IntoResponse {
    let crypt = match WeComCrypto::new(
        &state.config.token,
        &state.config.encoding_aes_key,
        &state.config.corp_id,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!("[wecom_callback] Crypto init failed: {e}");
            return (
                StatusCode::FORBIDDEN,
                "signature verification failed".to_string(),
            );
        }
    };

    match crypt.verify_url(
        &query.msg_signature,
        &query.timestamp,
        &query.nonce,
        &query.echostr,
    ) {
        Ok(plain) => (StatusCode::OK, plain),
        Err(e) => {
            warn!("[wecom_callback] URL verification failed: {e}");
            (
                StatusCode::FORBIDDEN,
                "signature verification failed".to_string(),
            )
        }
    }
}

async fn handle_callback(
    State(state): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
    body: String,
) -> impl IntoResponse {
    let crypt = match WeComCrypto::new(
        &state.config.token,
        &state.config.encoding_aes_key,
        &state.config.corp_id,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!("[wecom_callback] Crypto init failed: {e}");
            return (
                StatusCode::BAD_REQUEST,
                "invalid callback payload".to_string(),
            );
        }
    };

    let encrypt = match extract_encrypt_from_xml(&body) {
        Some(e) => e,
        None => {
            warn!("[wecom_callback] Missing Encrypt in XML body");
            return (
                StatusCode::BAD_REQUEST,
                "invalid callback payload".to_string(),
            );
        }
    };

    let decrypted = match crypt.decrypt(
        &query.msg_signature,
        &query.timestamp,
        &query.nonce,
        &encrypt,
    ) {
        Ok(xml) => xml,
        Err(e) => {
            warn!("[wecom_callback] Decrypt failed: {e}");
            return (
                StatusCode::BAD_REQUEST,
                "invalid callback payload".to_string(),
            );
        }
    };

    let decrypted_str = String::from_utf8_lossy(&decrypted);
    let event = match parse_xml_event(&decrypted_str, &state.config.corp_id) {
        Some(ev) => ev,
        None => {
            // Silently acknowledge lifecycle events or unsupported types
            return (StatusCode::OK, "success".to_string());
        }
    };

    // Deduplicate
    if !event.message_id.is_empty() && state.dedup.is_duplicate(&event.message_id) {
        debug!(
            "[wecom_callback] Duplicate MsgId {}, skipping",
            event.message_id
        );
        return (StatusCode::OK, "success".to_string());
    }
    if !event.message_id.is_empty() {
        state.dedup.is_duplicate(&event.message_id); // track it
    }

    // Echo prevention: ignore messages from the bot itself
    if event.user_id == state.config.corp_id {
        debug!("[wecom_callback] Ignoring echo from bot itself");
        return (StatusCode::OK, "success".to_string());
    }

    info!(
        "[wecom_callback] inbound from {}: {}",
        event.user_id,
        &event.content[..event.content.len().min(80)]
    );

    let chat_id = event.chat_id.clone();
    let content = event.content.clone();
    let handler = state.handler.clone();
    let running = state.running.clone();
    let running_sessions = state.running_sessions.clone();
    let busy_ack_ts = state.busy_ack_ts.clone();
    let session_store = state.session_store.clone();
    let _default_model = state.default_model.clone();
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
            sessions
                .get(&chat_id)
                .map(|&start_ts| (now - start_ts) / 60.0)
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
                let handler_guard = handler.lock().await;
                if let Some(h) = handler_guard.as_ref() {
                    h.interrupt(&chat_id, &content);
                }
                drop(handler_guard);
                info!("Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min");
                let busy_msg = format!(
                    "Still processing your previous message ({:.0}m elapsed). \
                     Please wait for my response before sending another prompt.",
                    elapsed_min
                );
                let _ = state_for_send.send_text(&chat_id, &busy_msg).await;
            }
            return;
        }

        let handler_guard = handler.lock().await;
        let Some(handler_ref) = handler_guard.as_ref().cloned() else {
            warn!("No message handler registered for WeCom callback messages");
            return;
        };
        drop(handler_guard);

        {
            let mut sessions = running_sessions.lock();
            sessions.insert(chat_id.clone(), now);
        }

        let model_override = per_chat_model.lock().get(&chat_id).cloned();
        match handler_ref
            .handle_message(
                Platform::WecomCallback,
                &chat_id,
                &content,
                model_override.as_deref(),
            )
            .await
        {
            Ok(result) => {
                running_sessions.lock().remove(&chat_id);
                busy_ack_ts.lock().remove(&chat_id);

                if result.compression_exhausted {
                    let source = SessionSource {
                        platform: Platform::WecomCallback,
                        chat_id: chat_id.clone(),
                        chat_name: None,
                        chat_type: "dm".to_string(),
                        user_id: Some(chat_id.clone()),
                        user_name: None,
                        thread_id: None,
                        chat_topic: None,
                        user_id_alt: None,
                        chat_id_alt: None,
                    };
                    session_store.reset_session_for(&source);
                    let _ = state_for_send
                        .send_text(
                            &chat_id,
                            "Session reset: conversation context grew too large. Starting fresh.",
                        )
                        .await;
                }
                if !result.response.is_empty() {
                    let _ = state_for_send.send_text(&chat_id, &result.response).await;
                }
            }
            Err(e) => {
                running_sessions.lock().remove(&chat_id);
                busy_ack_ts.lock().remove(&chat_id);
                error!("Agent handler failed for WeCom callback message: {e}");
                let _ = state_for_send
                    .send_text(
                        &chat_id,
                        "Sorry, I encountered an error processing your message.",
                    )
                    .await;
            }
        }
    });

    (StatusCode::OK, "success".to_string())
}

// ── XML parsing ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct InboundEvent {
    message_id: String,
    chat_id: String,
    user_id: String,
    content: String,
}

fn extract_encrypt_from_xml(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            }
            Ok(Event::Text(e)) => {
                if current_tag == "Encrypt" {
                    return Some(e.unescape().unwrap_or_default().into_owned());
                }
            }
            Ok(Event::CData(e)) => {
                if current_tag == "Encrypt" {
                    return Some(String::from_utf8_lossy(e.as_ref()).into_owned());
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

fn parse_xml_event(xml: &str, corp_id: &str) -> Option<InboundEvent> {
    let mut msg_type = String::new();
    let mut event_name = String::new();
    let mut user_id = String::new();
    let mut to_user_name = String::new();
    let mut content = String::new();
    let mut msg_id = String::new();
    let mut create_time = String::new();

    let mut reader = Reader::from_str(xml);

    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().into_owned();
                match current_tag.as_str() {
                    "MsgType" => msg_type = text.to_lowercase(),
                    "Event" => event_name = text.to_lowercase(),
                    "FromUserName" => user_id = text,
                    "ToUserName" => to_user_name = text,
                    "Content" => content = text.trim().to_string(),
                    "MsgId" => msg_id = text,
                    "CreateTime" => create_time = text,
                    _ => {}
                }
            }
            Ok(Event::CData(e)) => {
                let text = String::from_utf8_lossy(e.as_ref()).into_owned();
                match current_tag.as_str() {
                    "MsgType" => msg_type = text.to_lowercase(),
                    "Event" => event_name = text.to_lowercase(),
                    "FromUserName" => user_id = text,
                    "ToUserName" => to_user_name = text,
                    "Content" => content = text.trim().to_string(),
                    "MsgId" => msg_id = text,
                    "CreateTime" => create_time = text,
                    _ => {}
                }
            }
            Ok(Event::End(_)) => {
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // Silently acknowledge lifecycle events
    if msg_type == "event" && (event_name == "enter_agent" || event_name == "subscribe") {
        return None;
    }

    if msg_type != "text" && msg_type != "event" {
        return None;
    }

    if content.is_empty() && msg_type == "event" {
        content = "/start".to_string();
    }

    if user_id.is_empty() {
        return None;
    }

    let scoped_corp_id = if to_user_name.is_empty() {
        corp_id.to_string()
    } else {
        to_user_name
    };

    let chat_id = format!("{}:{}", scoped_corp_id, user_id);
    let message_id = if msg_id.is_empty() {
        format!("{}:{}", user_id, create_time)
    } else {
        msg_id
    };

    Some(InboundEvent {
        message_id,
        chat_id,
        user_id,
        content,
    })
}

// ── WeCom crypto ───────────────────────────────────────────────────────────

struct WeComCrypto {
    token: String,
    key: Vec<u8>,
    iv: Vec<u8>,
    receive_id: String,
}

#[derive(Debug)]
struct CryptoError(String);

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl WeComCrypto {
    fn new(token: &str, encoding_aes_key: &str, receive_id: &str) -> Result<Self, CryptoError> {
        if token.is_empty() {
            return Err(CryptoError("token is required".into()));
        }
        if encoding_aes_key.is_empty() {
            return Err(CryptoError("encoding_aes_key is required".into()));
        }
        if encoding_aes_key.len() != 43 {
            return Err(CryptoError("encoding_aes_key must be 43 chars".into()));
        }
        if receive_id.is_empty() {
            return Err(CryptoError("receive_id is required".into()));
        }

        let key = general_purpose::STANDARD
            .decode(format!("{encoding_aes_key}="))
            .map_err(|e| CryptoError(format!("invalid base64 aes_key: {e}")))?;
        if key.len() != 32 {
            return Err(CryptoError(format!(
                "invalid AES key length: expected 32, got {}",
                key.len()
            )));
        }
        let iv = key[..16].to_vec();

        Ok(Self {
            token: token.to_string(),
            key,
            iv,
            receive_id: receive_id.to_string(),
        })
    }

    fn verify_url(
        &self,
        msg_signature: &str,
        timestamp: &str,
        nonce: &str,
        echostr: &str,
    ) -> Result<String, CryptoError> {
        let plain = self.decrypt(msg_signature, timestamp, nonce, echostr)?;
        String::from_utf8(plain).map_err(|e| CryptoError(format!("invalid utf-8: {e}")))
    }

    fn decrypt(
        &self,
        msg_signature: &str,
        timestamp: &str,
        nonce: &str,
        encrypt: &str,
    ) -> Result<Vec<u8>, CryptoError> {
        let expected = sha1_signature(&self.token, timestamp, nonce, encrypt);
        if expected != msg_signature {
            return Err(CryptoError("signature mismatch".into()));
        }

        let cipher_text = general_purpose::STANDARD
            .decode(encrypt)
            .map_err(|e| CryptoError(format!("invalid base64 payload: {e}")))?;

        use aes::Aes256;
        use cbc::cipher::{BlockDecryptMut, KeyIvInit};

        type Aes256CbcDec = cbc::Decryptor<Aes256>;
        let dec = Aes256CbcDec::new_from_slices(&self.key, &self.iv)
            .map_err(|e| CryptoError(format!("invalid key/IV: {e}")))?;
        let mut buf = cipher_text.to_vec();
        let pt = dec
            .decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buf)
            .map_err(|e| CryptoError(format!("decryption failed: {e}")))?;

        // Skip 16-byte random prefix
        if pt.len() < 20 {
            return Err(CryptoError("decrypted payload too short".into()));
        }
        let content = &pt[16..];

        // Read 4-byte network-length
        let xml_length =
            u32::from_be_bytes([content[0], content[1], content[2], content[3]]) as usize;
        if 4 + xml_length > content.len() {
            return Err(CryptoError("invalid xml length".into()));
        }
        let xml_content = content[4..4 + xml_length].to_vec();
        let receive_id = String::from_utf8_lossy(&content[4 + xml_length..]);

        if receive_id != self.receive_id {
            return Err(CryptoError("receive_id mismatch".into()));
        }

        Ok(xml_content)
    }
}

fn sha1_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    let mut parts = [token, timestamp, nonce, encrypt];
    parts.sort_unstable();
    let data = parts.join("");
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = WecomCallbackConfig::default();
        assert_eq!(config.host, DEFAULT_HOST);
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.path, DEFAULT_PATH);
    }

    #[test]
    fn test_config_is_configured() {
        let mut config = WecomCallbackConfig::default();
        assert!(!config.is_configured());
        config.token = "t".into();
        config.encoding_aes_key = "k".repeat(43);
        config.corp_id = "c".into();
        config.corp_secret = "s".into();
        assert!(config.is_configured());
    }

    #[test]
    fn test_sha1_signature() {
        let sig = sha1_signature("token", "timestamp", "nonce", "echostr");
        assert_eq!(sig.len(), 40);
    }

    #[test]
    fn test_parse_xml_event_text() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[corp]]></ToUserName>
            <FromUserName><![CDATA[user123]]></FromUserName>
            <CreateTime>123456789</CreateTime>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[hello world]]></Content>
            <MsgId>msg001</MsgId>
        </xml>"#;
        let ev = parse_xml_event(xml, "corp").unwrap();
        assert_eq!(ev.user_id, "user123");
        assert_eq!(ev.content, "hello world");
        assert_eq!(ev.message_id, "msg001");
        assert_eq!(ev.chat_id, "corp:user123");
    }

    #[test]
    fn test_parse_xml_event_lifecycle() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[corp]]></ToUserName>
            <FromUserName><![CDATA[user123]]></FromUserName>
            <CreateTime>123456789</CreateTime>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[enter_agent]]></Event>
        </xml>"#;
        assert!(parse_xml_event(xml, "corp").is_none());
    }

    #[test]
    fn test_parse_xml_event_event_start() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[corp]]></ToUserName>
            <FromUserName><![CDATA[user123]]></FromUserName>
            <CreateTime>123456789</CreateTime>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[subscribe]]></Event>
        </xml>"#;
        assert!(parse_xml_event(xml, "corp").is_none());
    }

    #[test]
    fn test_crypto_new_invalid_key_length() {
        let result = WeComCrypto::new("token", "short", "corp");
        assert!(result.is_err());
    }
}
