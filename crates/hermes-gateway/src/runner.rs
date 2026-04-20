//! Gateway runner entry point for messaging platform integrations.
//!
//! Manages the gateway lifecycle:
//! - Loads platform configuration
//! - Starts configured platform adapters (Feishu, Weixin)
//! - Routes messages to the agent engine
//! - Handles graceful shutdown

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use crate::config::{Platform, PlatformConfig};
use crate::platforms::api_server::{ApiServerAdapter, ApiServerConfig, ApiServerState};
use crate::session::{SessionSource, SessionStore, build_session_key};
use crate::platforms::dingtalk::{DingtalkAdapter, DingtalkConfig};
use crate::platforms::discord::{DiscordAdapter, DiscordConfig};
use crate::platforms::email::{EmailAdapter, EmailConfig, EmailMessageEvent};
use crate::platforms::feishu::{FeishuAdapter, FeishuConfig, FeishuConnectionMode, FeishuMessageEvent};
use crate::platforms::slack::{SlackAdapter, SlackConfig, SlackMessageEvent};
use crate::platforms::telegram::{TelegramAdapter, TelegramConfig, TelegramMessageEvent};
use crate::platforms::webhook::{WebhookAdapter, WebhookConfig};
use crate::platforms::wecom::{WeComAdapter, WeComConfig};
use crate::platforms::weixin::{WeixinAdapter, WeixinConfig, WeixinMessageEvent};
use crate::platforms::qqbot::{QqbotAdapter, QqbotConfig, QqbotMessageEvent};
use crate::platforms::whatsapp::{WhatsAppAdapter, WhatsAppConfig, WhatsAppMessageEvent};
use crate::platforms::sms::{SmsAdapter, SmsConfig};
use crate::platforms::matrix::{MatrixAdapter, MatrixConfig};
use crate::platforms::homeassistant::{HomeAssistantAdapter, HomeAssistantConfig};
use crate::platforms::mattermost::{MattermostAdapter, MattermostConfig};
use crate::platforms::signal::{SignalAdapter, SignalConfig};
use crate::platforms::bluebubbles::{BlueBubblesAdapter, BlueBubblesConfig};
use crate::platforms::wecom_callback::{WecomCallbackAdapter, WecomCallbackConfig};

/// Gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Platform configurations.
    pub platforms: Vec<PlatformConfigEntry>,
    /// Default model to use.
    pub default_model: String,
}

/// A platform configuration entry with its enabled status.
#[derive(Debug, Clone)]
pub struct PlatformConfigEntry {
    pub platform: Platform,
    pub enabled: bool,
    pub config: PlatformConfig,
}

/// Result from a message handler, including metadata for gateway-level handling.
#[derive(Debug, Clone)]
pub struct HandlerResult {
    /// Response text to send to the user.
    pub response: String,
    /// Complete agent message history after the turn (includes tool_calls).
    /// Mirrors Python result["messages"] — used by Responses API to produce
    /// function_call/function_call_output output items.
    pub messages: Vec<serde_json::Value>,
    /// Compression was exhausted — gateway should auto-reset the session
    /// to break the infinite loop. Mirrors Python PR c5688e7c.
    pub compression_exhausted: bool,
    /// Token usage from the LLM response (if available).
    pub usage: Option<TokenUsage>,
}

/// Token usage info from the LLM.
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Message handler trait -- called when a platform receives a message.
#[async_trait::async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    async fn handle_message(
        &self,
        platform: Platform,
        chat_id: &str,
        content: &str,
        model_override: Option<&str>,
    ) -> Result<HandlerResult, String>;

    /// Signal the handler to interrupt its current conversation turn.
    /// Default is no-op for handlers that don't support interruption.
    /// Mirrors Python PR a8b7db35 — immediate interrupt on user message.
    fn interrupt(&self, _chat_id: &str, _new_message: &str) {
        // no-op by default
    }
}

/// Shared state for the health check endpoint.
#[derive(Clone)]
struct HealthCheckStatus {
    running: Arc<AtomicBool>,
    feishu: bool,
    weixin: bool,
    telegram: bool,
    discord: bool,
    slack: bool,
    api_server: bool,
    dingtalk: bool,
    wecom: bool,
    whatsapp: bool,
    webhook: bool,
    qqbot: bool,
    email: bool,
    sms: bool,
    matrix: bool,
    homeassistant: bool,
    mattermost: bool,
    signal: bool,
    bluebubbles: bool,
    wecom_callback: bool,
}

/// Health check HTTP handler.
async fn health_handler(
    axum::extract::State(status): axum::extract::State<Arc<HealthCheckStatus>>,
) -> axum::Json<serde_json::Value> {
    let mut platforms = serde_json::Map::new();
    platforms.insert("feishu".into(), serde_json::json!(status.feishu));
    platforms.insert("weixin".into(), serde_json::json!(status.weixin));
    platforms.insert("telegram".into(), serde_json::json!(status.telegram));
    platforms.insert("discord".into(), serde_json::json!(status.discord));
    platforms.insert("slack".into(), serde_json::json!(status.slack));
    platforms.insert("api_server".into(), serde_json::json!(status.api_server));
    platforms.insert("dingtalk".into(), serde_json::json!(status.dingtalk));
    platforms.insert("wecom".into(), serde_json::json!(status.wecom));
    platforms.insert("whatsapp".into(), serde_json::json!(status.whatsapp));
    platforms.insert("webhook".into(), serde_json::json!(status.webhook));
    platforms.insert("qqbot".into(), serde_json::json!(status.qqbot));
    platforms.insert("email".into(), serde_json::json!(status.email));
    platforms.insert("sms".into(), serde_json::json!(status.sms));
    platforms.insert("matrix".into(), serde_json::json!(status.matrix));
    platforms.insert("homeassistant".into(), serde_json::json!(status.homeassistant));
    platforms.insert("mattermost".into(), serde_json::json!(status.mattermost));
    platforms.insert("signal".into(), serde_json::json!(status.signal));
    platforms.insert("bluebubbles".into(), serde_json::json!(status.bluebubbles));
    platforms.insert("wecom_callback".into(), serde_json::json!(status.wecom_callback));

    let body = serde_json::json!({
        "status": if status.running.load(Ordering::SeqCst) { "ok" } else { "stopped" },
        "platforms": platforms,
    });
    axum::Json(body)
}

/// Gateway runner managing platform adapter lifecycles.
pub struct GatewayRunner {
    config: GatewayConfig,
    feishu_adapter: Option<Arc<FeishuAdapter>>,
    weixin_adapter: Option<Arc<WeixinAdapter>>,
    telegram_adapter: Option<Arc<TelegramAdapter>>,
    discord_adapter: Option<Arc<DiscordAdapter>>,
    slack_adapter: Option<Arc<SlackAdapter>>,
    api_server_adapter: Option<Arc<ApiServerAdapter>>,
    dingtalk_adapter: Option<Arc<DingtalkAdapter>>,
    wecom_adapter: Option<Arc<WeComAdapter>>,
    whatsapp_adapter: Option<Arc<WhatsAppAdapter>>,
    webhook_adapter: Option<Arc<WebhookAdapter>>,
    qqbot_adapter: Option<Arc<QqbotAdapter>>,
    email_adapter: Option<Arc<EmailAdapter>>,
    sms_adapter: Option<Arc<SmsAdapter>>,
    matrix_adapter: Option<Arc<MatrixAdapter>>,
    homeassistant_adapter: Option<Arc<HomeAssistantAdapter>>,
    mattermost_adapter: Option<Arc<MattermostAdapter>>,
    signal_adapter: Option<Arc<SignalAdapter>>,
    bluebubbles_adapter: Option<Arc<BlueBubblesAdapter>>,
    wecom_callback_adapter: Option<Arc<WecomCallbackAdapter>>,
    api_server_shutdown_tx: Vec<oneshot::Sender<()>>,
    dingtalk_shutdown_tx: Vec<oneshot::Sender<()>>,
    feishu_shutdown_tx: Vec<oneshot::Sender<()>>,
    telegram_shutdown_tx: Vec<oneshot::Sender<()>>,
    discord_shutdown_tx: Vec<oneshot::Sender<()>>,
    slack_shutdown_tx: Vec<oneshot::Sender<()>>,
    whatsapp_shutdown_tx: Vec<oneshot::Sender<()>>,
    webhook_shutdown_tx: Vec<oneshot::Sender<()>>,
    email_shutdown_tx: Vec<oneshot::Sender<()>>,
    sms_shutdown_tx: Vec<oneshot::Sender<()>>,
    homeassistant_shutdown_tx: Vec<oneshot::Sender<()>>,
    bluebubbles_shutdown_tx: Vec<oneshot::Sender<()>>,
    wecom_callback_shutdown_tx: Vec<oneshot::Sender<()>>,
    // Matrix doesn't use oneshot shutdown — the sync loop checks running AtomicBool
    /// Health check server shutdown sender.
    health_check_shutdown_tx: Option<oneshot::Sender<()>>,
    message_handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<AtomicBool>,
    /// Track which sessions are currently running (chat_id -> start timestamp).
    /// Used for busy-session interrupt logic (Python PR a8b7db35).
    /// std::sync::Mutex — critical sections are trivially fast (HashMap insert/get).
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    /// Busy ack timestamps for debouncing (chat_id -> last ack time).
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    /// Session store for persistence and auto-reset.
    session_store: Arc<SessionStore>,
    /// Per-chat model overrides (set via /model command).
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
}

impl GatewayRunner {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            feishu_adapter: None,
            weixin_adapter: None,
            telegram_adapter: None,
            discord_adapter: None,
            slack_adapter: None,
            api_server_adapter: None,
            dingtalk_adapter: None,
            wecom_adapter: None,
            whatsapp_adapter: None,
            webhook_adapter: None,
            qqbot_adapter: None,
            email_adapter: None,
            sms_adapter: None,
            matrix_adapter: None,
            homeassistant_adapter: None,
            mattermost_adapter: None,
            signal_adapter: None,
            bluebubbles_adapter: None,
            wecom_callback_adapter: None,
            api_server_shutdown_tx: Vec::new(),
            dingtalk_shutdown_tx: Vec::new(),
            feishu_shutdown_tx: Vec::new(),
            telegram_shutdown_tx: Vec::new(),
            discord_shutdown_tx: Vec::new(),
            slack_shutdown_tx: Vec::new(),
            whatsapp_shutdown_tx: Vec::new(),
            webhook_shutdown_tx: Vec::new(),
            email_shutdown_tx: Vec::new(),
            sms_shutdown_tx: Vec::new(),
            homeassistant_shutdown_tx: Vec::new(),
            bluebubbles_shutdown_tx: Vec::new(),
            wecom_callback_shutdown_tx: Vec::new(),
            health_check_shutdown_tx: None,
            message_handler: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            running_sessions: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            busy_ack_ts: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            session_store: Arc::new(SessionStore::new(
                hermes_core::get_hermes_home().join("gateway_sessions"),
                crate::config::GatewayConfig::default(),
            )),
            per_chat_model: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Set the message handler (agent engine).
    pub async fn set_message_handler(&self, handler: Arc<dyn MessageHandler>) {
        *self.message_handler.lock().await = Some(handler);
    }

    /// Initialize platform adapters based on config.
    pub fn initialize(&mut self) {
        for entry in &self.config.platforms {
            if !entry.enabled {
                info!("Platform {} disabled, skipping", entry.platform.as_str());
                continue;
            }
            match entry.platform {
                Platform::Feishu => {
                    let feishu_config = FeishuConfig::from_env();
                    if !feishu_config.app_id.is_empty() && !feishu_config.app_secret.is_empty() {
                        info!("Initializing Feishu adapter...");
                        self.feishu_adapter = Some(Arc::new(FeishuAdapter::new(feishu_config)));
                    } else {
                        warn!("Feishu enabled but not configured (missing FEISHU_APP_ID/SECRET)");
                    }
                }
                Platform::Weixin => {
                    let weixin_config = WeixinConfig::from_env();
                    if !weixin_config.session_key.is_empty() {
                        info!("Initializing Weixin adapter...");
                        self.weixin_adapter = Some(Arc::new(WeixinAdapter::new(weixin_config)));
                    } else {
                        warn!("Weixin enabled but not configured (missing WEIXIN_SESSION_KEY)");
                    }
                }
                Platform::Telegram => {
                    let telegram_config = TelegramConfig::from_env();
                    if !telegram_config.bot_token.is_empty() {
                        info!("Initializing Telegram adapter...");
                        self.telegram_adapter = Some(Arc::new(TelegramAdapter::new(telegram_config)));
                    } else {
                        warn!("Telegram enabled but not configured (missing TELEGRAM_BOT_TOKEN)");
                    }
                }
                Platform::Discord => {
                    let discord_config = DiscordConfig::from_env();
                    if !discord_config.bot_token.is_empty() {
                        info!("Initializing Discord adapter...");
                        self.discord_adapter = Some(Arc::new(DiscordAdapter::new(discord_config)));
                    } else {
                        warn!("Discord enabled but not configured (missing DISCORD_BOT_TOKEN)");
                    }
                }
                Platform::Slack => {
                    let slack_config = SlackConfig::from_env();
                    if !slack_config.bot_token.is_empty() && !slack_config.signing_secret.is_empty() {
                        info!("Initializing Slack adapter...");
                        self.slack_adapter = Some(Arc::new(SlackAdapter::new(slack_config)));
                    } else {
                        warn!("Slack enabled but not configured (missing SLACK_BOT_TOKEN or SLACK_SIGNING_SECRET)");
                    }
                }
                Platform::ApiServer => {
                    let api_config = ApiServerConfig::from_env();
                    info!(
                        "Initializing API Server adapter on {}:{}...",
                        api_config.host, api_config.port
                    );
                    self.api_server_adapter = Some(Arc::new(ApiServerAdapter::new(api_config)));
                }
                Platform::Dingtalk => {
                    let dingtalk_config = DingtalkConfig::from_extra(&entry.config.extra);
                    if !dingtalk_config.client_id.is_empty() && !dingtalk_config.client_secret.is_empty() {
                        let mode_str = match dingtalk_config.connection_mode {
                            crate::platforms::dingtalk::DingtalkConnectionMode::Stream => "Stream",
                            crate::platforms::dingtalk::DingtalkConnectionMode::Webhook => "Webhook",
                        };
                        info!("Initializing Dingtalk adapter ({mode_str} mode)...");
                        self.dingtalk_adapter =
                            Some(Arc::new(DingtalkAdapter::new(dingtalk_config)));
                    } else {
                        warn!(
                            "Dingtalk enabled but not configured \
                             (missing DINGTALK_CLIENT_ID/SECRET)"
                        );
                    }
                }
                Platform::Wecom => {
                    let wecom_config = WeComConfig::from_env();
                    if !wecom_config.bot_id.is_empty() && !wecom_config.secret.is_empty() {
                        info!("Initializing WeCom adapter...");
                        self.wecom_adapter = Some(Arc::new(WeComAdapter::new(wecom_config)));
                    } else {
                        warn!(
                            "WeCom enabled but not configured \
                             (missing WECOM_BOT_ID/SECRET)"
                        );
                    }
                }
                Platform::Whatsapp => {
                    let whatsapp_config = WhatsAppConfig::from_env();
                    if !whatsapp_config.bridge_script.is_empty() {
                        info!("Initializing WhatsApp adapter...");
                        self.whatsapp_adapter = Some(Arc::new(WhatsAppAdapter::new(whatsapp_config)));
                    } else {
                        warn!("WhatsApp enabled but not configured (missing bridge script)");
                    }
                }
                Platform::Webhook => {
                    let webhook_config = WebhookConfig::from_env();
                    info!("Initializing Webhook adapter...");
                    self.webhook_adapter = Some(Arc::new(WebhookAdapter::new(webhook_config)));
                }
                Platform::Qqbot => {
                    let qqbot_config = QqbotConfig::from_env();
                    if !qqbot_config.app_id.is_empty() && !qqbot_config.client_secret.is_empty() {
                        info!("Initializing QQ Bot adapter...");
                        self.qqbot_adapter = Some(Arc::new(QqbotAdapter::new(qqbot_config)));
                    } else {
                        warn!("QQ Bot enabled but not configured (missing QQ_APP_ID/SECRET)");
                    }
                }
                Platform::Email => {
                    let email_config = EmailConfig::from_env();
                    if email_config.is_configured() {
                        info!("Initializing Email adapter...");
                        self.email_adapter = Some(Arc::new(EmailAdapter::new(email_config)));
                    } else {
                        warn!("Email enabled but not configured (missing EMAIL_ADDRESS/PASSWORD/IMAP/SMTP)");
                    }
                }
                Platform::Sms => {
                    let sms_config = SmsConfig::from_env();
                    if sms_config.is_configured() {
                        info!("Initializing SMS adapter...");
                        self.sms_adapter = Some(Arc::new(SmsAdapter::new(sms_config)));
                    } else {
                        warn!("SMS enabled but not configured (missing TWILIO_ACCOUNT_SID/TOKEN/PHONE_NUMBER)");
                    }
                }
                Platform::Matrix => {
                    let matrix_config = MatrixConfig::from_extra(&entry.config.extra);
                    if matrix_config.is_configured() {
                        info!("Initializing Matrix adapter...");
                        self.matrix_adapter = Some(Arc::new(MatrixAdapter::new(matrix_config)));
                    } else {
                        warn!("Matrix enabled but not configured (missing MATRIX_HOMESERVER + access token or password)");
                    }
                }
                Platform::Homeassistant => {
                    let config = HomeAssistantConfig::from_env();
                    if !config.hass_url.is_empty() && !config.hass_token.is_empty() {
                        info!("Initializing Home Assistant adapter...");
                        self.homeassistant_adapter = Some(Arc::new(HomeAssistantAdapter::new(config)));
                    } else {
                        warn!("Home Assistant enabled but not configured (missing HASS_URL/HASS_TOKEN)");
                    }
                }
                Platform::Mattermost => {
                    let config = MattermostConfig::from_env();
                    if !config.token.is_empty() && !config.server_url.is_empty() {
                        info!("Initializing Mattermost adapter...");
                        self.mattermost_adapter = Some(Arc::new(MattermostAdapter::new(config)));
                    } else {
                        warn!("Mattermost enabled but not configured (missing MATTERMOST_SERVER_URL/TOKEN)");
                    }
                }
                Platform::Signal => {
                    let config = SignalConfig::from_env();
                    if !config.phone_number.is_empty() && !config.signal_http_url.is_empty() {
                        info!("Initializing Signal adapter...");
                        self.signal_adapter = Some(Arc::new(SignalAdapter::new(config)));
                    } else {
                        warn!("Signal enabled but not configured (missing SIGNAL_PHONE_NUMBER/HTTP_URL)");
                    }
                }
                Platform::Bluebubbles => {
                    let config = BlueBubblesConfig::from_env();
                    if !config.server_url.is_empty() && !config.password.is_empty() {
                        info!("Initializing BlueBubbles adapter...");
                        self.bluebubbles_adapter = Some(Arc::new(BlueBubblesAdapter::new(config)));
                    } else {
                        warn!("BlueBubbles enabled but not configured (missing BLUEBUBBLES_SERVER_URL/PASSWORD)");
                    }
                }
                Platform::WecomCallback => {
                    let config = WecomCallbackConfig::from_env();
                    if !config.corp_id.is_empty() && !config.token.is_empty() && !config.encoding_aes_key.is_empty() {
                        info!("Initializing WeCom callback adapter...");
                        self.wecom_callback_adapter = Some(Arc::new(WecomCallbackAdapter::new(config)));
                    } else {
                        warn!("WeCom callback enabled but not configured (missing WECOM_CALLBACK_CORP_ID/TOKEN/ENCODING_AES_KEY)");
                    }
                }
                _ => {
                    warn!("Platform {} not yet implemented in Rust", entry.platform.as_str());
                }
            }
        }

        let feishu_count = self.feishu_adapter.is_some() as usize;
        let weixin_count = self.weixin_adapter.is_some() as usize;
        let telegram_count = self.telegram_adapter.is_some() as usize;
        let discord_count = self.discord_adapter.is_some() as usize;
        let slack_count = self.slack_adapter.is_some() as usize;
        let api_server_count = self.api_server_adapter.is_some() as usize;
        let dingtalk_count = self.dingtalk_adapter.is_some() as usize;
        let wecom_count = self.wecom_adapter.is_some() as usize;
        let whatsapp_count = self.whatsapp_adapter.is_some() as usize;
        let webhook_count = self.webhook_adapter.is_some() as usize;
        let qqbot_count = self.qqbot_adapter.is_some() as usize;
        let email_count = self.email_adapter.is_some() as usize;
        let sms_count = self.sms_adapter.is_some() as usize;
        let matrix_count = self.matrix_adapter.is_some() as usize;
        let homeassistant_count = self.homeassistant_adapter.is_some() as usize;
        let mattermost_count = self.mattermost_adapter.is_some() as usize;
        let signal_count = self.signal_adapter.is_some() as usize;
        let bluebubbles_count = self.bluebubbles_adapter.is_some() as usize;
        let wecom_callback_count = self.wecom_callback_adapter.is_some() as usize;
        let feishu_webhook_count = self.feishu_adapter.as_ref()
            .map(|a| matches!(a.config.connection_mode, FeishuConnectionMode::Webhook))
            .unwrap_or(false) as usize;
        info!(
            "Gateway initialized: {} platform(s) ready",
            feishu_count + weixin_count + telegram_count + discord_count + slack_count + api_server_count + dingtalk_count + wecom_count + whatsapp_count + webhook_count + qqbot_count + email_count + sms_count + matrix_count + homeassistant_count + mattermost_count + signal_count + bluebubbles_count + wecom_callback_count
        );
        if feishu_webhook_count > 0 {
            info!("Feishu webhook: port={} path={}",
                self.feishu_adapter.as_ref().unwrap().config.webhook_port,
                self.feishu_adapter.as_ref().unwrap().config.webhook_path
            );
        }
    }

    /// Start the gateway main loop.
    pub async fn run(&mut self) -> Result<(), String> {
        self.running.store(true, Ordering::SeqCst);
        info!("Gateway starting...");

        // Spawn platform-specific polling tasks
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        if let Some(adapter) = &self.weixin_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let handle = tokio::spawn(async move {
                run_weixin_poll(adapter, handler, running, running_sessions, busy_ack_ts, session_store, default_model, per_chat_model).await;
            });
            handles.push(handle);
        }

        // Telegram: start polling loop or webhook server
        if let Some(adapter) = &self.telegram_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let is_webhook = adapter.config.webhook_url.is_some();

            if is_webhook {
                let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                let adapter_for_webhook = adapter.clone();
                let handle = tokio::spawn(async move {
                    let on_msg = move |event: TelegramMessageEvent| {
                        let handler = handler.clone();
                        let running = running.clone();
                        let running_sessions = running_sessions.clone();
                        let busy_ack_ts = busy_ack_ts.clone();
                        let session_store = session_store.clone();
                        let default_model = default_model.clone();
                        let per_chat_model = per_chat_model.clone();
                        let adapter = adapter.clone();
                        tokio::spawn(async move {
                            if !running.load(Ordering::SeqCst) {
                                return;
                            }
                            let handler_guard = handler.lock().await;
                            let handler_ref = handler_guard.as_ref().cloned();
                            drop(handler_guard);
                            route_telegram_message(
                                &adapter,
                                handler_ref.as_ref(),
                                &event,
                                &running_sessions,
                                &busy_ack_ts,
                                &session_store,
                                &default_model,
                                &per_chat_model,
                            )
                            .await;
                        });
                    };
                    if let Err(e) = adapter_for_webhook.run_webhook(on_msg, shutdown_rx).await {
                        error!("Telegram webhook error: {e}");
                    }
                });
                self.telegram_shutdown_tx.push(shutdown_tx);
                handles.push(handle);
            } else {
                let handle = tokio::spawn(async move {
                    run_telegram_poll(adapter, handler, running, running_sessions, busy_ack_ts, session_store, default_model, per_chat_model).await;
                });
                handles.push(handle);
            }
        }

        // Discord: start Gateway WebSocket loop
        if let Some(adapter) = &self.discord_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                Arc::clone(&adapter).run(handler, running).await;
            });
            self.discord_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // Slack: start Event API webhook server
        if let Some(adapter) = &self.slack_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let adapter_for_run = adapter.clone();
            let handle = tokio::spawn(async move {
                let on_msg = move |event: SlackMessageEvent| {
                    let handler = handler.clone();
                    let running = running.clone();
                    let adapter = adapter.clone();
                    let running_sessions = running_sessions.clone();
                    let busy_ack_ts = busy_ack_ts.clone();
                    let session_store = session_store.clone();
                    let default_model = default_model.clone();
                    let per_chat_model = per_chat_model.clone();
                    tokio::spawn(async move {
                        if !running.load(Ordering::SeqCst) {
                            return;
                        }
                        let guard = handler.lock().await;
                        if let Some(h) = guard.as_ref() {
                            let chat_id = &event.channel_id;
                            let content = &event.content;
                            info!(
                                "Slack message from {} via {}: {}",
                                event.user_id,
                                chat_id,
                                content.chars().take(50).collect::<String>(),
                            );

                            // Check busy session
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs_f64();
                            let is_busy = {
                                let sessions = running_sessions.lock();
                                sessions.contains_key(chat_id)
                            };

                            if is_busy {
                                // Allow /stop even when the session is busy
                                if GatewayCommand::parse(content)
                                    .map(|c| c.name == "/stop")
                                    .unwrap_or(false)
                                {
                                    let ctx = command_ctx(
                                        Platform::Slack, chat_id, "channel",
                                        Some(event.user_id.clone()), event.thread_ts.clone(),
                                        &session_store, &running_sessions, &busy_ack_ts,
                                        &default_model, &per_chat_model, Some(h),
                                    );
                                    if let Some(reply) = try_handle_command(&ctx, content).await {
                                        let _ = adapter.send_text(chat_id, &reply).await;
                                    }
                                    return;
                                }

                                let should_ack = {
                                    let mut ack_map = busy_ack_ts.lock();
                                    let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
                                    if now - last_ack < 30.0 {
                                        false
                                    } else {
                                        ack_map.insert(chat_id.to_string(), now);
                                        true
                                    }
                                };
                                if should_ack {
                                    h.interrupt(chat_id, content);
                                    let _ = adapter.send_text(chat_id,
                                        "Still processing your previous message. Please wait.").await;
                                }
                                return;
                            }

                            // Command detection before agent invocation
                            let ctx = command_ctx(
                                Platform::Slack, chat_id, if event.is_dm { "dm" } else { "channel" },
                                Some(event.user_id.clone()), event.thread_ts.clone(),
                                &session_store, &running_sessions, &busy_ack_ts,
                                &default_model, &per_chat_model, Some(h),
                            );
                            if let Some(reply) = try_handle_command(&ctx, content).await {
                                let _ = adapter.send_text(chat_id, &reply).await;
                                return;
                            }

                            {
                                let mut sessions = running_sessions.lock();
                                sessions.insert(chat_id.clone(), now);
                            }

                            let model_override = per_chat_model.lock().get(chat_id).cloned();
                            match h.handle_message(Platform::Slack, chat_id, content, model_override.as_deref()).await {
                                Ok(result) => {
                                    running_sessions.lock().remove(chat_id);
                                    busy_ack_ts.lock().remove(chat_id);

                                    if result.compression_exhausted {
                                        let source = SessionSource {
                                            platform: Platform::Slack,
                                            chat_id: chat_id.to_string(),
                                            chat_name: None,
                                            chat_type: if event.is_dm { "dm".to_string() } else { "channel".to_string() },
                                            user_id: Some(event.user_id.clone()),
                                            user_name: None,
                                            thread_id: event.thread_ts.clone(),
                                            chat_topic: None,
                                            user_id_alt: None,
                                            chat_id_alt: None,
                                        };
                                        session_store.reset_session_for(&source);
                                        let _ = adapter.send_text(chat_id,
                                            "Session reset: conversation context grew too large. Starting fresh.").await;
                                    }
                                    if !result.response.is_empty() {
                                        let target = if let Some(ref ts) = event.thread_ts {
                                            adapter.send_text_in_thread(chat_id, &result.response, ts).await
                                        } else {
                                            adapter.send_text(chat_id, &result.response).await
                                        };
                                        if let Err(e) = target {
                                            error!("Slack send failed: {e}");
                                        }
                                    }
                                }
                                Err(e) => {
                                    running_sessions.lock().remove(chat_id);
                                    busy_ack_ts.lock().remove(chat_id);
                                    error!("Agent handler failed for Slack message: {e}");
                                    let _ = adapter.send_text(chat_id,
                                        "Sorry, I encountered an error processing your message.").await;
                                }
                            }
                        }
                    });
                };
                if let Err(e) = adapter_for_run.run(on_msg, shutdown_rx).await {
                    error!("Slack webhook error: {e}");
                }
            });
            self.slack_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // Feishu: start webhook server (Webhook mode) or log WebSocket mode
        if let Some(adapter) = &self.feishu_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();

            match adapter.config.connection_mode {
                FeishuConnectionMode::Webhook => {
                    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                    let handle = tokio::spawn(async move {
                        // Set up the on_message callback to route to handler
                        let adapter_for_cb = adapter.clone();
                        adapter.on_message.write().await.replace(Arc::new(
                            move |event: FeishuMessageEvent| {
                                let handler = handler.clone();
                                let running = running.clone();
                                let adapter = adapter_for_cb.clone();
                                let running_sessions = running_sessions.clone();
                                let busy_ack_ts = busy_ack_ts.clone();
                                let session_store = session_store.clone();
                                let default_model = default_model.clone();
                                let per_chat_model = per_chat_model.clone();
                                tokio::spawn(async move {
                                    if !running.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    let guard = handler.lock().await;
                                    if let Some(h) = guard.as_ref() {
                                        let chat_id = &event.chat_id;
                                        let content = &event.content;
                                        info!(
                                            "Feishu message from {} via {}: {}",
                                            event.sender_id,
                                            chat_id,
                                            content.chars().take(50).collect::<String>(),
                                        );

                                        // Check busy session
                                        let now = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs_f64();
                                        let is_busy = {
                                            let sessions = running_sessions.lock();
                                            sessions.contains_key(chat_id)
                                        };

                                        if is_busy {
                                            // Allow /stop even when the session is busy
                                            if GatewayCommand::parse(content)
                                                .map(|c| c.name == "/stop")
                                                .unwrap_or(false)
                                            {
                                                let ctx = command_ctx(
                                                    Platform::Feishu, chat_id, if event.is_group { "group" } else { "dm" },
                                                    Some(event.sender_id.clone()), None,
                                                    &session_store, &running_sessions, &busy_ack_ts,
                                                    &default_model, &per_chat_model, Some(h),
                                                );
                                                if let Some(reply) = try_handle_command(&ctx, content).await {
                                                    let _ = adapter.send_text(chat_id, &reply).await;
                                                }
                                                return;
                                            }

                                            let should_ack = {
                                                let mut ack_map = busy_ack_ts.lock();
                                                let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
                                                if now - last_ack < 30.0 {
                                                    false
                                                } else {
                                                    ack_map.insert(chat_id.to_string(), now);
                                                    true
                                                }
                                            };
                                            if should_ack {
                                                h.interrupt(chat_id, content);
                                                let _ = adapter.send_text(chat_id,
                                                    "Still processing your previous message. Please wait.").await;
                                            }
                                            return;
                                        }

                                        // Command detection before agent invocation
                                        let ctx = command_ctx(
                                            Platform::Feishu, chat_id, if event.is_group { "group" } else { "dm" },
                                            Some(event.sender_id.clone()), None,
                                            &session_store, &running_sessions, &busy_ack_ts,
                                            &default_model, &per_chat_model, Some(h),
                                        );
                                        if let Some(reply) = try_handle_command(&ctx, content).await {
                                            let _ = adapter.send_text_or_post(chat_id, &reply).await;
                                            return;
                                        }

                                        {
                                            let mut sessions = running_sessions.lock();
                                            sessions.insert(chat_id.clone(), now);
                                        }

                                        let model_override = per_chat_model.lock().get(chat_id).cloned();
                                        match h
                                            .handle_message(
                                                Platform::Feishu,
                                                chat_id,
                                                content,
                                                model_override.as_deref(),
                                            )
                                            .await
                                        {
                                            Ok(result) => {
                                                running_sessions.lock().remove(chat_id);
                                                busy_ack_ts.lock().remove(chat_id);

                                                if result.compression_exhausted {
                                                    let source = SessionSource {
                                                        platform: Platform::Feishu,
                                                        chat_id: chat_id.to_string(),
                                                        chat_name: None,
                                                        chat_type: if event.is_group { "group".to_string() } else { "dm".to_string() },
                                                        user_id: Some(event.sender_id.clone()),
                                                        user_name: None,
                                                        thread_id: None,
                                                        chat_topic: None,
                                                        user_id_alt: None,
                                                        chat_id_alt: None,
                                                    };
                                                    session_store.reset_session_for(&source);
                                                    let _ = adapter.send_text(chat_id,
                                                        "Session reset: conversation context grew too large. Starting fresh.").await;
                                                }
                                                if !result.response.is_empty() {
                                                    if let Err(e) =
                                                        adapter.send_text_or_post(chat_id, &result.response).await
                                                    {
                                                        error!("Feishu send failed: {e}");
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                running_sessions.lock().remove(chat_id);
                                                busy_ack_ts.lock().remove(chat_id);
                                                error!("Agent handler failed for Feishu message: {e}");
                                                let _ = adapter
                                                    .send_text(
                                                        chat_id,
                                                        "Sorry, I encountered an error processing your message.",
                                                    )
                                                    .await;
                                            }
                                        }
                                    }
                                });
                            },
                        ));

                        if let Err(e) = adapter.run_webhook(shutdown_rx).await {
                            error!("Feishu webhook error: {e}");
                        }
                    });
                    self.feishu_shutdown_tx.push(shutdown_tx);
                    handles.push(handle);
                }
                FeishuConnectionMode::WebSocket => {
                    let ws_client = crate::platforms::feishu_ws::FeishuWsClient::new(adapter.config.clone());
                    let adapter = adapter.clone();
                    let handle = tokio::spawn(async move {
                        let callback: crate::platforms::feishu_ws::WsEventCallback = std::sync::Arc::new(move |event: serde_json::Value| {
                            let adapter = adapter.clone();
                            tokio::spawn(async move {
                                adapter.process_ws_event(event).await;
                            });
                        });
                        ws_client.run(callback).await;
                    });
                    handles.push(handle);
                }
            }
        }

        // API Server: start HTTP server
        if let Some(adapter) = &self.api_server_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let api_key = adapter.config.api_key.clone();
            let model_name = adapter.config.model_name.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                let state = ApiServerState {
                    handler,
                    api_key,
                    model_name,
                };
                if let Err(e) = adapter.run(state, shutdown_rx).await {
                    error!("API Server error: {e}");
                }
            });
            self.api_server_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // WeCom: start WebSocket connection
        if let Some(adapter) = &self.wecom_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let handle = tokio::spawn(async move {
                adapter.run(handler, running).await;
            });
            handles.push(handle);
        }

        // Dingtalk: start stream or webhook depending on config
        if let Some(adapter) = &self.dingtalk_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            match adapter.config.connection_mode {
                crate::platforms::dingtalk::DingtalkConnectionMode::Stream => {
                    let running = self.running.clone();
                    let handle = tokio::spawn(async move {
                        adapter.run_stream(handler, running).await;
                    });
                    handles.push(handle);
                }
                crate::platforms::dingtalk::DingtalkConnectionMode::Webhook => {
                    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                    let handle = tokio::spawn(async move {
                        if let Err(e) = adapter.run(handler, shutdown_rx).await {
                            error!("Dingtalk webhook error: {e}");
                        }
                    });
                    self.dingtalk_shutdown_tx.push(shutdown_tx);
                    handles.push(handle);
                }
            }
        }

        // WhatsApp: connect bridge and start polling
        if let Some(adapter) = &self.whatsapp_adapter {
            let adapter = adapter.clone();
            if let Err(e) = adapter.connect().await {
                error!("WhatsApp connect failed: {e}");
            } else {
                let adapter = adapter.clone();
                let handler = self.message_handler.clone();
                let running = self.running.clone();
                let running_sessions = self.running_sessions.clone();
                let busy_ack_ts = self.busy_ack_ts.clone();
                let session_store = self.session_store.clone();
                let default_model = self.config.default_model.clone();
                let per_chat_model = self.per_chat_model.clone();
                let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
                let handle = tokio::spawn(async move {
                    run_whatsapp_poll(adapter, handler, running, running_sessions, busy_ack_ts, session_store, default_model, per_chat_model).await;
                });
                self.whatsapp_shutdown_tx.push(shutdown_tx);
                handles.push(handle);
            }
        }

        // Webhook: start HTTP server
        if let Some(adapter) = &self.webhook_adapter {
            if let Err(e) = adapter.validate_routes().await {
                error!("Webhook validation failed: {e}");
            } else {
                let adapter = adapter.clone();
                let handler = self.message_handler.clone();
                let running = self.running.clone();
                let running_sessions = self.running_sessions.clone();
                let busy_ack_ts = self.busy_ack_ts.clone();
                let session_store = self.session_store.clone();
                let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                let handle = tokio::spawn(async move {
                    if let Err(e) = adapter.run(handler, running, running_sessions, busy_ack_ts, session_store, shutdown_rx).await {
                        error!("Webhook server error: {e}");
                    }
                });
                self.webhook_shutdown_tx.push(shutdown_tx);
                handles.push(handle);
            }
        }

        // QQ Bot: start WebSocket event stream
        if let Some(adapter) = &self.qqbot_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let handle = tokio::spawn(async move {
                run_qqbot(adapter, handler, running, running_sessions, busy_ack_ts, session_store, default_model, per_chat_model).await;
            });
            handles.push(handle);
        }

        // Matrix: connect and start sync loop
        if let Some(adapter) = &self.matrix_adapter {
            let adapter = adapter.clone();
            if let Err(e) = adapter.connect().await {
                error!("Matrix connect failed: {e}");
            } else {
                let handler = self.message_handler.clone();
                let running = self.running.clone();
                let running_sessions = self.running_sessions.clone();
                let busy_ack_ts = self.busy_ack_ts.clone();
                let session_store = self.session_store.clone();
                let default_model = self.config.default_model.clone();
                let per_chat_model = self.per_chat_model.clone();
                let handle = tokio::spawn(async move {
                    adapter.run(handler, running, running_sessions, busy_ack_ts, session_store, default_model, per_chat_model).await;
                });
                handles.push(handle);
            }
        }

        // Home Assistant: start WebSocket listener
        if let Some(adapter) = &self.homeassistant_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                adapter.run(handler, running, shutdown_rx).await;
            });
            self.homeassistant_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // Mattermost: start WebSocket listener
        if let Some(adapter) = &self.mattermost_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let handle = tokio::spawn(async move {
                adapter.run(handler, running).await;
            });
            handles.push(handle);
        }

        // Signal: start SSE listener
        if let Some(adapter) = &self.signal_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let handle = tokio::spawn(async move {
                adapter.run(handler, running).await;
            });
            handles.push(handle);
        }

        // BlueBubbles: start webhook server
        if let Some(adapter) = &self.bluebubbles_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                if let Err(e) = adapter.run(handler, running, running_sessions, busy_ack_ts, session_store, shutdown_rx, default_model, per_chat_model).await {
                    error!("BlueBubbles webhook error: {e}");
                }
            });
            self.bluebubbles_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // WeCom callback: start HTTP server
        if let Some(adapter) = &self.wecom_callback_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                if let Err(e) = adapter.run(handler, running, running_sessions, busy_ack_ts, session_store, shutdown_rx, default_model, per_chat_model).await {
                    error!("WeCom callback server error: {e}");
                }
            });
            self.wecom_callback_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // SMS: start webhook server
        if let Some(adapter) = &self.sms_adapter {
            let adapter = adapter.clone();
            let handler = self.message_handler.clone();
            let running = self.running.clone();
            let running_sessions = self.running_sessions.clone();
            let busy_ack_ts = self.busy_ack_ts.clone();
            let session_store = self.session_store.clone();
            let default_model = self.config.default_model.clone();
            let per_chat_model = self.per_chat_model.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                if let Err(e) = adapter.run(
                    handler, running, running_sessions, busy_ack_ts,
                    session_store, shutdown_rx, default_model, per_chat_model,
                ).await {
                    error!("SMS webhook error: {e}");
                }
            });
            self.sms_shutdown_tx.push(shutdown_tx);
            handles.push(handle);
        }

        // Email: connect and start polling loop
        if let Some(adapter) = &self.email_adapter {
            let adapter = adapter.clone();
            if let Err(e) = adapter.connect().await {
                error!("Email connect failed: {e}");
            } else {
                let adapter = adapter.clone();
                let handler = self.message_handler.clone();
                let running = self.running.clone();
                let running_sessions = self.running_sessions.clone();
                let busy_ack_ts = self.busy_ack_ts.clone();
                let session_store = self.session_store.clone();
                let default_model = self.config.default_model.clone();
                let per_chat_model = self.per_chat_model.clone();
                let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
                let handle = tokio::spawn(async move {
                    run_email_poll(adapter, handler, running, running_sessions, busy_ack_ts, session_store, default_model, per_chat_model).await;
                });
                self.email_shutdown_tx.push(shutdown_tx);
                handles.push(handle);
            }
        }

        // Health check endpoint
        let health_port = std::env::var("HERMES_GATEWAY_HEALTH_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let health_status = HealthCheckStatus {
            running: self.running.clone(),
            feishu: self.feishu_adapter.is_some(),
            weixin: self.weixin_adapter.is_some(),
            telegram: self.telegram_adapter.is_some(),
            discord: self.discord_adapter.is_some(),
            slack: self.slack_adapter.is_some(),
            api_server: self.api_server_adapter.is_some(),
            dingtalk: self.dingtalk_adapter.is_some(),
            wecom: self.wecom_adapter.is_some(),
            whatsapp: self.whatsapp_adapter.is_some(),
            webhook: self.webhook_adapter.is_some(),
            qqbot: self.qqbot_adapter.is_some(),
            email: self.email_adapter.is_some(),
            sms: self.sms_adapter.is_some(),
            matrix: self.matrix_adapter.is_some(),
            homeassistant: self.homeassistant_adapter.is_some(),
            mattermost: self.mattermost_adapter.is_some(),
            signal: self.signal_adapter.is_some(),
            bluebubbles: self.bluebubbles_adapter.is_some(),
            wecom_callback: self.wecom_callback_adapter.is_some(),
        };
        let (health_shutdown_tx, health_shutdown_rx) = oneshot::channel::<()>();
        let health_handle = tokio::spawn(async move {
            let app = axum::Router::new()
                .route("/health", axum::routing::get(health_handler))
                .with_state(Arc::new(health_status));

            let listener = match tokio::net::TcpListener::bind(("0.0.0.0", health_port)).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("Health check bind failed on port {health_port}: {e}");
                    return;
                }
            };
            info!("Health check endpoint on http://0.0.0.0:{health_port}/health");

            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = health_shutdown_rx.await;
                })
                .await;
        });
        self.health_check_shutdown_tx = Some(health_shutdown_tx);
        handles.push(health_handle);

        // Wait for all platform tasks
        for handle in handles {
            if let Err(e) = handle.await {
                error!("Platform task panicked: {e}");
            }
        }

        info!("Gateway stopped");
        Ok(())
    }

    /// Stop the gateway gracefully.
    pub fn stop(&mut self) {
        // Trigger API server graceful shutdown
        let senders = std::mem::take(&mut self.api_server_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger Dingtalk webhook graceful shutdown
        let senders = std::mem::take(&mut self.dingtalk_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Disconnect Dingtalk stream adapter
        if let Some(adapter) = self.dingtalk_adapter.take() {
            tokio::spawn(async move {
                adapter.disconnect().await;
            });
        }
        // Trigger Feishu webhook graceful shutdown
        let senders = std::mem::take(&mut self.feishu_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger Telegram graceful shutdown
        let senders = std::mem::take(&mut self.telegram_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger Discord graceful shutdown
        let senders = std::mem::take(&mut self.discord_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger Slack graceful shutdown
        let senders = std::mem::take(&mut self.slack_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger WhatsApp graceful shutdown
        let senders = std::mem::take(&mut self.whatsapp_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Disconnect WhatsApp adapter
        if let Some(adapter) = self.whatsapp_adapter.take() {
            tokio::spawn(async move {
                adapter.disconnect().await;
            });
        }
        // Trigger Webhook graceful shutdown
        let senders = std::mem::take(&mut self.webhook_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger SMS graceful shutdown
        let senders = std::mem::take(&mut self.sms_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger Email graceful shutdown
        let senders = std::mem::take(&mut self.email_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Disconnect Email adapter
        if let Some(adapter) = self.email_adapter.take() {
            adapter.disconnect();
        }
        // Trigger BlueBubbles graceful shutdown
        let senders = std::mem::take(&mut self.bluebubbles_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger WeCom callback graceful shutdown
        let senders = std::mem::take(&mut self.wecom_callback_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger Home Assistant graceful shutdown
        let senders = std::mem::take(&mut self.homeassistant_shutdown_tx);
        for tx in senders {
            let _ = tx.send(());
        }
        // Trigger health check server graceful shutdown
        if let Some(tx) = self.health_check_shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.running.store(false, Ordering::SeqCst);
        // Clear tracking state so it doesn't leak across stop/restart cycles.
        self.running_sessions.lock().clear();
        self.busy_ack_ts.lock().clear();
        info!("Gateway stop requested");
    }

    /// Check if the gateway is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get status information.
    pub fn status(&self) -> GatewayStatus {
        GatewayStatus {
            running: self.is_running(),
            feishu_configured: self.feishu_adapter.is_some(),
            weixin_configured: self.weixin_adapter.is_some(),
            telegram_configured: self.telegram_adapter.is_some(),
            discord_configured: self.discord_adapter.is_some(),
            slack_configured: self.slack_adapter.is_some(),
            api_server_configured: self.api_server_adapter.is_some(),
            dingtalk_configured: self.dingtalk_adapter.is_some(),
            wecom_configured: self.wecom_adapter.is_some(),
            whatsapp_configured: self.whatsapp_adapter.is_some(),
            webhook_configured: self.webhook_adapter.is_some(),
            qqbot_configured: self.qqbot_adapter.is_some(),
            email_configured: self.email_adapter.is_some(),
            sms_configured: self.sms_adapter.is_some(),
            matrix_configured: self.matrix_adapter.is_some(),
            homeassistant_configured: self.homeassistant_adapter.is_some(),
            mattermost_configured: self.mattermost_adapter.is_some(),
            signal_configured: self.signal_adapter.is_some(),
            bluebubbles_configured: self.bluebubbles_adapter.is_some(),
            wecom_callback_configured: self.wecom_callback_adapter.is_some(),
            platform_count: self.config.platforms.iter().filter(|p| p.enabled).count(),
        }
    }
}

/// Gateway status information.
#[derive(Debug, Clone)]
pub struct GatewayStatus {
    pub running: bool,
    pub feishu_configured: bool,
    pub weixin_configured: bool,
    pub telegram_configured: bool,
    pub discord_configured: bool,
    pub slack_configured: bool,
    pub api_server_configured: bool,
    pub dingtalk_configured: bool,
    pub wecom_configured: bool,
    pub whatsapp_configured: bool,
    pub webhook_configured: bool,
    pub qqbot_configured: bool,
    pub email_configured: bool,
    pub sms_configured: bool,
    pub matrix_configured: bool,
    pub homeassistant_configured: bool,
    pub mattermost_configured: bool,
    pub signal_configured: bool,
    pub bluebubbles_configured: bool,
    pub wecom_callback_configured: bool,
    pub platform_count: usize,
}

// ── Gateway Commands ───────────────────────────────────────────────────────

/// Parsed gateway command from a user message.
#[derive(Debug, Clone)]
struct GatewayCommand {
    name: String,
    args: Vec<String>,
    raw: String,
}

impl GatewayCommand {
    /// Parse a message text into a command if it starts with '/'.
    fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }
        let name = parts[0].to_lowercase();
        let args = parts[1..].iter().map(|s| s.to_string()).collect();
        Some(Self {
            name,
            args,
            raw: trimmed.to_string(),
        })
    }
}

/// Context passed to command handlers.
struct CommandContext<'a> {
    session_source: SessionSource,
    session_store: &'a SessionStore,
    running_sessions: &'a Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &'a Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    default_model: &'a str,
    per_chat_model: &'a Arc<parking_lot::Mutex<HashMap<String, String>>>,
    handler: Option<&'a Arc<dyn MessageHandler>>,
}

/// Build a `CommandContext` from raw platform fields.
fn command_ctx<'a>(
    platform: Platform,
    chat_id: &'a str,
    chat_type: &'a str,
    user_id: Option<String>,
    thread_id: Option<String>,
    session_store: &'a SessionStore,
    running_sessions: &'a Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &'a Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    default_model: &'a str,
    per_chat_model: &'a Arc<parking_lot::Mutex<HashMap<String, String>>>,
    handler: Option<&'a Arc<dyn MessageHandler>>,
) -> CommandContext<'a> {
    CommandContext {
        session_source: SessionSource {
            platform,
            chat_id: chat_id.to_string(),
            chat_name: None,
            chat_type: chat_type.to_string(),
            user_id,
            user_name: None,
            thread_id,
            chat_topic: None,
            user_id_alt: None,
            chat_id_alt: None,
        },
        session_store,
        running_sessions,
        busy_ack_ts,
        default_model,
        per_chat_model,
        handler,
    }
}

/// Try to handle a gateway command. Returns `Some(reply)` if the message was a
/// command and has been handled (caller should send the reply and skip the
/// normal agent handler). Returns `None` for normal messages.
async fn try_handle_command(ctx: &CommandContext<'_>, content: &str) -> Option<String> {
    let cmd = GatewayCommand::parse(content)?;

    let chat_id = ctx.session_source.chat_id.clone();

    match cmd.name.as_str() {
        "/reset" | "/new" | "/restart" => {
            ctx.session_store.reset_session_for(&ctx.session_source);
            // Also clear any per-chat model override
            ctx.per_chat_model.lock().remove(&chat_id);
            Some("✅ Session reset. Starting fresh!".to_string())
        }

        "/stop" => {
            let was_running = {
                let mut sessions = ctx.running_sessions.lock();
                sessions.remove(&chat_id).is_some()
            };
            if was_running {
                // Also clear busy ack timestamp
                ctx.busy_ack_ts.lock().remove(&chat_id);
                // Signal interrupt to the running agent
                if let Some(h) = ctx.handler {
                    h.interrupt(&chat_id, "/stop");
                }
                Some("⏹️ Stopped the current conversation.".to_string())
            } else {
                Some("ℹ️ No active conversation to stop.".to_string())
            }
        }

        "/status" => {
            let session_key = build_session_key(
                &ctx.session_source,
                ctx.session_store.group_sessions_per_user(),
                ctx.session_store.thread_sessions_per_user(),
            );
            let sessions = ctx.session_store.list_sessions(None);
            let session_info = sessions.iter().find(|s| s.session_key == session_key);

            let model_override = ctx.per_chat_model.lock().get(&chat_id).cloned();
            let active_model = model_override.as_deref().unwrap_or(ctx.default_model);

            let mut lines = vec![
                "*Gateway Status*".to_string(),
                String::new(),
                format!("Platform: {}", ctx.session_source.platform.as_str()),
                format!("Active model: {active_model}"),
            ];

            if let Some(entry) = session_info {
                lines.push(format!("Session ID: {}", entry.session_id));
                lines.push(format!(
                    "Messages: {} tokens",
                    entry.total_tokens
                ));
                lines.push(format!(
                    "Last active: {}",
                    entry.updated_at.format("%Y-%m-%d %H:%M:%S")
                ));
                if entry.was_auto_reset {
                    lines.push(format!(
                        "Auto-reset: {} ({})",
                        entry.auto_reset_reason.as_deref().unwrap_or("unknown"),
                        if entry.reset_had_activity { "had activity" } else { "no activity" }
                    ));
                }
            } else {
                lines.push("Session: new (no history yet)".to_string());
            }

            let running = {
                let sessions = ctx.running_sessions.lock();
                sessions.contains_key(&chat_id)
            };
            lines.push(format!("Agent state: {}", if running { "🟢 running" } else { "⚪ idle" }));

            Some(lines.join("\n"))
        }

        "/model" => {
            if cmd.args.is_empty() {
                let current = ctx
                    .per_chat_model
                    .lock()
                    .get(&chat_id)
                    .cloned()
                    .unwrap_or_else(|| ctx.default_model.to_string());
                Some(format!("Current model: {current}\nUsage: /model <model-name>\n\
                    ⚠️ Note: per-chat model override is not yet wired to the agent engine."))
            } else {
                let model = cmd.args[0].clone();
                ctx.per_chat_model
                    .lock()
                    .insert(chat_id, model.clone());
                Some(format!("✅ Model set to: {model}\n\
                    ⚠️ Note: per-chat model override is not yet wired to the agent engine."))
            }
        }

        "/help" | "/commands" => {
            let help_text = "Available commands:\n\
                • /reset or /new — Reset the current session\n\
                • /stop — Stop the current conversation\n\
                • /status — Show gateway and session status\n\
                • /model [name] — Show or set the model for this chat\n\
                • /help — Show this help message";
            Some(help_text.to_string())
        }

        // Unknown command — treat as normal message so the agent can handle it
        _ => None,
    }
}

/// Poll Weixin for inbound messages and route to the agent.
async fn run_weixin_poll(
    adapter: Arc<WeixinAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    let mut poll_interval = interval(Duration::from_secs(2));
    let mut consecutive_errors = 0u32;

    info!("Weixin poll loop started");

    while running.load(Ordering::SeqCst) {
        poll_interval.tick().await;

        match adapter.get_updates().await {
            Ok(events) => {
                consecutive_errors = 0;
                for event in events {
                    // Check busy + interrupt before acquiring handler lock.
                    // This lets us call interrupt() on the handler Arc
                    // without needing to hold the Mutex guard.
                    let handler_guard = handler.lock().await;
                    let handler_ref = handler_guard.as_ref().cloned();
                    drop(handler_guard); // Release lock before routing

                    route_weixin_message(
                        &adapter, handler_ref.as_ref(), &event,
                        &running_sessions, &busy_ack_ts, &session_store,
                        &default_model, &per_chat_model,
                    ).await;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if e.contains("session expired") {
                    error!("Weixin session expired, pausing for 10 minutes");
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    consecutive_errors = 0;
                    continue;
                }
                if consecutive_errors > 5 {
                    warn!("Weixin: {consecutive_errors} consecutive errors: {e}");
                } else {
                    error!("Weixin poll error: {e}");
                }
                // Backoff on errors
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    info!("Weixin poll loop stopped");
}

/// Route a Weixin message to the agent handler.
///
/// If the session is already running (agent is busy), interrupt the agent,
/// send a busy ack to the user, and queue the message for the next cycle.
/// Mirrors Python PR a8b7db35 — immediate interrupt on user message.
async fn route_weixin_message(
    adapter: &WeixinAdapter,
    handler: Option<&Arc<dyn MessageHandler>>,
    event: &WeixinMessageEvent,
    running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: &Arc<SessionStore>,
    default_model: &str,
    per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    if event.content.is_empty() {
        return;
    }

    // DM / Group policy check (mirrors Python `_process_message`)
    let chat_id = &event.peer_id;
    if event.is_group {
        if !adapter.is_group_allowed(chat_id) {
            debug!("Weixin group message from {chat_id} blocked by policy");
            return;
        }
    } else if !adapter.is_dm_allowed(chat_id) {
        debug!("Weixin DM from {chat_id} blocked by policy");
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Check if this session is already running (busy session handling)
    let busy_elapsed_min: Option<f64> = {
        let sessions = running_sessions.lock();
        sessions.get(chat_id).map(|&start_ts| {
            let elapsed_secs = now - start_ts;
            elapsed_secs / 60.0
        })
    };

    if let Some(elapsed_min) = busy_elapsed_min {
        // Allow /stop even when the session is busy
        if GatewayCommand::parse(&event.content)
            .map(|c| c.name == "/stop")
            .unwrap_or(false)
        {
            let ctx = command_ctx(
                Platform::Weixin, chat_id, if event.is_group { "group" } else { "dm" },
                None, None,
                session_store, running_sessions, busy_ack_ts,
                default_model, per_chat_model, handler,
            );
            if let Some(reply) = try_handle_command(&ctx, &event.content).await {
                let _ = adapter.send_text(chat_id, &reply).await;
            }
            return;
        }

        // Session is busy — interrupt the running agent and ack

        // Busy ack debounce: only send every 30 seconds
        let should_ack = {
            let mut ack_map = busy_ack_ts.lock();
            let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
            if now - last_ack < 30.0 {
                false // Debounced
            } else {
                ack_map.insert(chat_id.to_string(), now);
                true
            }
        };

        if should_ack {
            // Signal interrupt to the running agent
            if let Some(h) = handler {
                h.interrupt(chat_id, &event.content);
            }
            info!(
                "Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min"
            );

            // Send busy status to user
            let busy_msg = format!(
                "Still processing your previous message ({elapsed_min:.0}m elapsed). \
                 Please wait for my response before sending another prompt."
            );
            let _ = adapter.send_text(chat_id, &busy_msg).await;
        }
        return;
    }

    // Session not running — proceed with normal handling
    info!(
        "Weixin message from {}: {}",
        chat_id,
        event.content.chars().take(50).collect::<String>(),
    );

    // Command detection before agent invocation
    let ctx = command_ctx(
        Platform::Weixin, chat_id, if event.is_group { "group" } else { "dm" },
        None, None,
        session_store, running_sessions, busy_ack_ts,
        default_model, per_chat_model, handler,
    );
    if let Some(reply) = try_handle_command(&ctx, &event.content).await {
        let _ = adapter.send_text(chat_id, &reply).await;
        return;
    }

    // Mark session as running
    {
        let mut sessions = running_sessions.lock();
        sessions.insert(chat_id.clone(), now);
    }

    let Some(handler_ref) = handler else {
        running_sessions.lock().remove(chat_id);
        warn!("No message handler registered for Weixin messages");
        return;
    };

    let model_override = per_chat_model.lock().get(chat_id).cloned();
    match handler_ref
        .handle_message(Platform::Weixin, chat_id, &event.content, model_override.as_deref())
        .await
    {
        Ok(result) => {
            // Clear session running flag
            running_sessions.lock().remove(chat_id);
            // Clear busy ack timestamp
            busy_ack_ts.lock().remove(chat_id);

            // Compression exhaustion — auto-reset session and notify user.
            // Mirrors Python gateway/run.py behavior.
            if result.compression_exhausted {
                let source = SessionSource {
                    platform: Platform::Weixin,
                    chat_id: chat_id.to_string(),
                    chat_name: None,
                    chat_type: if event.is_group { "group".to_string() } else { "dm".to_string() },
                    user_id: None,
                    user_name: None,
                    thread_id: None,
                    chat_topic: None,
                    user_id_alt: None,
                    chat_id_alt: None,
                };
                session_store.reset_session_for(&source);
                warn!("Session {chat_id}: compression exhausted — auto-reset performed");
                let reset_msg = "Session reset: conversation context grew too large. \
                    Starting fresh — previous context has been cleared.";
                let _ = adapter.send_text(chat_id, reset_msg).await;
            }
            if !result.response.is_empty() {
                if let Err(e) = adapter.send_text(chat_id, &result.response).await {
                    error!("Weixin send failed: {e}");
                }
            }
        }
        Err(e) => {
            // Clear session running flag on error too
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            error!("Agent handler failed for Weixin message: {e}");
            let _ = adapter
                .send_text(chat_id, "Sorry, I encountered an error processing your message.")
                .await;
        }
    }
}

/// Poll Telegram for inbound messages and route to the agent.
async fn run_telegram_poll(
    adapter: Arc<TelegramAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    let mut poll_interval = interval(Duration::from_secs(1));
    let mut consecutive_errors = 0u32;

    info!("Telegram poll loop started");

    while running.load(Ordering::SeqCst) {
        poll_interval.tick().await;

        match adapter.get_updates().await {
            Ok(events) => {
                consecutive_errors = 0;
                for event in events {
                    let handler_guard = handler.lock().await;
                    let handler_ref = handler_guard.as_ref().cloned();
                    drop(handler_guard);

                    route_telegram_message(
                        &adapter,
                        handler_ref.as_ref(),
                        &event,
                        &running_sessions,
                        &busy_ack_ts,
                        &session_store,
                        &default_model,
                        &per_chat_model,
                    )
                    .await;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > 5 {
                    warn!("Telegram: {consecutive_errors} consecutive errors: {e}");
                } else {
                    error!("Telegram poll error: {e}");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    info!("Telegram poll loop stopped");
}

/// Route a Telegram message to the agent handler.
async fn route_telegram_message(
    adapter: &TelegramAdapter,
    handler: Option<&Arc<dyn MessageHandler>>,
    event: &TelegramMessageEvent,
    running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: &Arc<SessionStore>,
    default_model: &str,
    per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    if event.content.is_empty() && event.media.is_empty() {
        return;
    }

    let chat_id = &event.chat_id;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Check if this session is already running (busy session handling)
    let busy_elapsed_min: Option<f64> = {
        let sessions = running_sessions.lock();
        sessions.get(chat_id).map(|&start_ts| {
            let elapsed_secs = now - start_ts;
            elapsed_secs / 60.0
        })
    };

    if let Some(elapsed_min) = busy_elapsed_min {
        // Allow /stop even when the session is busy
        if GatewayCommand::parse(&event.content)
            .map(|c| c.name == "/stop")
            .unwrap_or(false)
        {
            let ctx = command_ctx(
                Platform::Telegram, chat_id, "dm",
                event.sender_id.clone(), event.message_thread_id.map(|id| id.to_string()),
                session_store, running_sessions, busy_ack_ts,
                default_model, per_chat_model, handler,
            );
            if let Some(reply) = try_handle_command(&ctx, &event.content).await {
                let _ = adapter.send_text(chat_id, &reply).await;
            }
            return;
        }

        let should_ack = {
            let mut ack_map = busy_ack_ts.lock();
            let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
            if now - last_ack < 30.0 {
                false
            } else {
                ack_map.insert(chat_id.to_string(), now);
                true
            }
        };

        if should_ack {
            if let Some(h) = handler {
                h.interrupt(chat_id, &event.content);
            }
            info!(
                "Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min"
            );

            let busy_msg = format!(
                "Still processing your previous message ({elapsed_min:.0}m elapsed). \
                 Please wait for my response before sending another prompt."
            );
            let _ = adapter.send_text(chat_id, &busy_msg).await;
        }
        return;
    }

    info!(
        "Telegram message from {}: {}",
        chat_id,
        event.content.chars().take(50).collect::<String>(),
    );

    // Command detection before agent invocation
    let ctx = command_ctx(
        Platform::Telegram, chat_id, "dm",
        event.sender_id.clone(), event.message_thread_id.map(|id| id.to_string()),
        session_store, running_sessions, busy_ack_ts,
        default_model, per_chat_model, handler,
    );
    if let Some(reply) = try_handle_command(&ctx, &event.content).await {
        let _ = adapter.send_text(chat_id, &reply).await;
        return;
    }

    {
        let mut sessions = running_sessions.lock();
        sessions.insert(chat_id.clone(), now);
    }

    let Some(handler_ref) = handler else {
        running_sessions.lock().remove(chat_id);
        warn!("No message handler registered for Telegram messages");
        return;
    };

    let model_override = per_chat_model.lock().get(chat_id).cloned();
    match handler_ref
        .handle_message(Platform::Telegram, chat_id, &event.content, model_override.as_deref())
        .await
    {
        Ok(result) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            // Compression exhaustion — auto-reset session and notify user.
            if result.compression_exhausted {
                let source = SessionSource {
                    platform: Platform::Telegram,
                    chat_id: chat_id.to_string(),
                    chat_name: None,
                    chat_type: "dm".to_string(),
                    user_id: event.sender_id.clone(),
                    user_name: None,
                    thread_id: event.message_thread_id.map(|id| id.to_string()),
                    chat_topic: None,
                    user_id_alt: None,
                    chat_id_alt: None,
                };
                session_store.reset_session_for(&source);
                warn!("Session {chat_id}: compression exhausted — auto-reset performed");
                let reset_msg = "Session reset: conversation context grew too large. \
                    Starting fresh — previous context has been cleared.";
                let _ = adapter.send_text(chat_id, reset_msg).await;
            }
            if !result.response.is_empty() {
                if let Err(e) = adapter.send_text(chat_id, &result.response).await {
                    error!("Telegram send failed: {e}");
                }
            }
        }
        Err(e) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            error!("Agent handler failed for Telegram message: {e}");
            let _ = adapter
                .send_text(chat_id, "Sorry, I encountered an error processing your message.")
                .await;
        }
    }
}

/// Load gateway config from config.yaml.
pub fn load_gateway_config() -> GatewayConfig {
    use hermes_core::hermes_home::get_hermes_home;

    let config_path = get_hermes_home().join("config.yaml");
    let mut platforms = Vec::new();
    let mut default_model = "gpt-4".to_string();

    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            // Read gateway config
            if let Some(gateway) = config.get("gateway") {
                if let Some(model) = gateway.get("default_model").and_then(|v| v.as_str()) {
                    default_model = model.to_string();
                }
                if let Some(platforms_cfg) = gateway.get("platforms") {
                    if let Some(arr) = platforms_cfg.as_sequence() {
                        for item in arr {
                            if let Some(platform_str) = item.get("platform").and_then(|v| v.as_str()) {
                                let enabled = item
                                    .get("enabled")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(true);
                                let platform = match platform_str {
                                    "feishu" => Platform::Feishu,
                                    "weixin" => Platform::Weixin,
                                    "wecom" => Platform::Wecom,
                                    "telegram" => Platform::Telegram,
                                    "discord" => Platform::Discord,
                                    "slack" => Platform::Slack,
                                    "api_server" => Platform::ApiServer,
                                    "whatsapp" => Platform::Whatsapp,
                                    "webhook" => Platform::Webhook,
                                    "email" => Platform::Email,
                                    "homeassistant" => Platform::Homeassistant,
                                    "mattermost" => Platform::Mattermost,
                                    "signal" => Platform::Signal,
                                    "bluebubbles" => Platform::Bluebubbles,
                                    "wecom_callback" => Platform::WecomCallback,
                                    _ => Platform::Local,
                                };
                                let cfg = PlatformConfig::default();
                                platforms.push(PlatformConfigEntry {
                                    platform,
                                    enabled,
                                    config: cfg,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: check env vars for enabled platforms
    if platforms.is_empty() {
        if std::env::var("FEISHU_APP_ID").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Feishu,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("WEIXIN_SESSION_KEY").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Weixin,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("API_SERVER_PORT").is_ok() || std::env::var("API_SERVER_KEY").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::ApiServer,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("DINGTALK_CLIENT_ID").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Dingtalk,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("WECOM_BOT_ID").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Wecom,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("TELEGRAM_BOT_TOKEN").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Telegram,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("WHATSAPP_BRIDGE_SCRIPT").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Whatsapp,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("DISCORD_BOT_TOKEN").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Discord,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("SLACK_BOT_TOKEN").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Slack,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("WEBHOOK_PORT").is_ok() || std::env::var("WEBHOOK_SECRET").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Webhook,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("EMAIL_ADDRESS").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Email,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("TWILIO_ACCOUNT_SID").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Sms,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("MATRIX_HOMESERVER").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Matrix,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("HASS_TOKEN").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Homeassistant,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("MATTERMOST_SERVER_URL").is_ok() || std::env::var("MATTERMOST_TOKEN").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Mattermost,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("SIGNAL_PHONE_NUMBER").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Signal,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("BLUEBUBBLES_SERVER_URL").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::Bluebubbles,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
        if std::env::var("WECOM_CALLBACK_TOKEN").is_ok() || std::env::var("WECOM_CALLBACK_CORP_ID").is_ok() {
            platforms.push(PlatformConfigEntry {
                platform: Platform::WecomCallback,
                enabled: true,
                config: PlatformConfig::default(),
            });
        }
    }

    GatewayConfig {
        platforms,
        default_model,
    }
}

/// Poll WhatsApp for inbound messages and route to the agent.
async fn run_whatsapp_poll(
    adapter: Arc<WhatsAppAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    let mut poll_interval = interval(Duration::from_secs(1));
    let mut consecutive_errors = 0u32;

    info!("WhatsApp poll loop started");

    while running.load(Ordering::SeqCst) {
        poll_interval.tick().await;

        match adapter.get_updates().await {
            Ok(events) => {
                consecutive_errors = 0;
                for event in events {
                    let handler_guard = handler.lock().await;
                    let handler_ref = handler_guard.as_ref().cloned();
                    drop(handler_guard);

                    route_whatsapp_message(
                        &adapter,
                        handler_ref.as_ref(),
                        &event,
                        &running_sessions,
                        &busy_ack_ts,
                        &session_store,
                        &default_model,
                        &per_chat_model,
                    )
                    .await;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > 5 {
                    warn!("WhatsApp: {consecutive_errors} consecutive errors: {e}");
                } else {
                    error!("WhatsApp poll error: {e}");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    info!("WhatsApp poll loop stopped");
}

/// Route a WhatsApp message to the agent handler.
async fn route_whatsapp_message(
    adapter: &WhatsAppAdapter,
    handler: Option<&Arc<dyn MessageHandler>>,
    event: &WhatsAppMessageEvent,
    running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: &Arc<SessionStore>,
    default_model: &str,
    per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    if event.content.is_empty() {
        return;
    }

    let chat_id = &event.chat_id;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Check if this session is already running (busy session handling)
    let busy_elapsed_min: Option<f64> = {
        let sessions = running_sessions.lock();
        sessions.get(chat_id).map(|start_ts| {
            let elapsed_secs = now - start_ts;
            elapsed_secs / 60.0
        })
    };

    if let Some(elapsed_min) = busy_elapsed_min {
        // Allow /stop even when the session is busy
        if GatewayCommand::parse(&event.content)
            .map(|c| c.name == "/stop")
            .unwrap_or(false)
        {
            let ctx = command_ctx(
                Platform::Whatsapp, chat_id, "dm",
                None, None,
                session_store, running_sessions, busy_ack_ts,
                default_model, per_chat_model, handler,
            );
            if let Some(reply) = try_handle_command(&ctx, &event.content).await {
                let _ = adapter.send_text(chat_id, &reply).await;
            }
            return;
        }

        let should_ack = {
            let mut ack_map = busy_ack_ts.lock();
            let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
            if now - last_ack < 30.0 {
                false
            } else {
                ack_map.insert(chat_id.to_string(), now);
                true
            }
        };

        if should_ack {
            if let Some(h) = handler {
                h.interrupt(chat_id, &event.content);
            }
            info!(
                "Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min"
            );

            let busy_msg = format!(
                "Still processing your previous message ({elapsed_min:.0}m elapsed). \
                 Please wait for my response before sending another prompt."
            );
            let _ = adapter.send_text(chat_id, &busy_msg).await;
        }
        return;
    }

    info!(
        "WhatsApp message from {}: {}",
        chat_id,
        event.content.chars().take(50).collect::<String>(),
    );

    // Command detection before agent invocation
    let ctx = command_ctx(
        Platform::Whatsapp, chat_id, "dm",
        None, None,
        session_store, running_sessions, busy_ack_ts,
        default_model, per_chat_model, handler,
    );
    if let Some(reply) = try_handle_command(&ctx, &event.content).await {
        let _ = adapter.send_text(chat_id, &reply).await;
        return;
    }

    {
        let mut sessions = running_sessions.lock();
        sessions.insert(chat_id.clone(), now);
    }

    let Some(handler_ref) = handler else {
        running_sessions.lock().remove(chat_id);
        warn!("No message handler registered for WhatsApp messages");
        return;
    };

    let model_override = per_chat_model.lock().get(chat_id).cloned();
    match handler_ref
        .handle_message(Platform::Whatsapp, chat_id, &event.content, model_override.as_deref())
        .await
    {
        Ok(result) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            if result.compression_exhausted {
                let source = SessionSource {
                    platform: Platform::Whatsapp,
                    chat_id: chat_id.to_string(),
                    chat_name: None,
                    chat_type: "dm".to_string(),
                    user_id: None,
                    user_name: None,
                    thread_id: None,
                    chat_topic: None,
                    user_id_alt: None,
                    chat_id_alt: None,
                };
                session_store.reset_session_for(&source);
                warn!("Session {chat_id}: compression exhausted — auto-reset performed");
                let reset_msg = "Session reset: conversation context grew too large. \
                    Starting fresh — previous context has been cleared.";
                let _ = adapter.send_text(chat_id, reset_msg).await;
            }
            if !result.response.is_empty() {
                if let Err(e) = adapter.send_text(chat_id, &result.response).await {
                    error!("WhatsApp send failed: {e}");
                }
            }
        }
        Err(e) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            error!("Agent handler failed for WhatsApp message: {e}");
            let _ = adapter
                .send_text(chat_id, "Sorry, I encountered an error processing your message.")
                .await;
        }
    }
}

/// Poll Email INBOX for new messages and route to the agent.
async fn run_email_poll(
    adapter: Arc<EmailAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    let mut poll_interval = interval(Duration::from_secs(adapter.poll_interval_secs()));
    let mut consecutive_errors = 0u32;

    info!("Email poll loop started");

    while running.load(Ordering::SeqCst) {
        poll_interval.tick().await;

        match adapter.get_updates().await {
            Ok(events) => {
                consecutive_errors = 0;
                for event in events {
                    let handler_guard = handler.lock().await;
                    let handler_ref = handler_guard.as_ref().cloned();
                    drop(handler_guard);

                    route_email_message(
                        &adapter,
                        handler_ref.as_ref(),
                        &event,
                        &running_sessions,
                        &busy_ack_ts,
                        &session_store,
                        &default_model,
                        &per_chat_model,
                    )
                    .await;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > 5 {
                    warn!("Email: {consecutive_errors} consecutive errors: {e}");
                } else {
                    error!("Email poll error: {e}");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    info!("Email poll loop stopped");
}

/// Route an Email message to the agent handler.
async fn route_email_message(
    adapter: &EmailAdapter,
    handler: Option<&Arc<dyn MessageHandler>>,
    event: &EmailMessageEvent,
    running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: &Arc<SessionStore>,
    default_model: &str,
    per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let chat_id = &event.chat_id;
    let content = &event.content;
    if content.is_empty() && event.media_paths.is_empty() {
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Check if this session is already running (busy session handling)
    let busy_elapsed_min: Option<f64> = {
        let sessions = running_sessions.lock();
        sessions.get(chat_id).map(|&start_ts| {
            let elapsed_secs = now - start_ts;
            elapsed_secs / 60.0
        })
    };

    if let Some(elapsed_min) = busy_elapsed_min {
        // Allow /stop even when the session is busy
        if GatewayCommand::parse(content)
            .map(|c| c.name == "/stop")
            .unwrap_or(false)
        {
            let ctx = command_ctx(
                Platform::Email, chat_id, "dm",
                Some(chat_id.clone()), None,
                session_store, running_sessions, busy_ack_ts,
                default_model, per_chat_model, handler,
            );
            if let Some(reply) = try_handle_command(&ctx, content).await {
                let _ = adapter.send_text(chat_id, &reply).await;
            }
            return;
        }

        let should_ack = {
            let mut ack_map = busy_ack_ts.lock();
            let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
            if now - last_ack < 30.0 {
                false
            } else {
                ack_map.insert(chat_id.to_string(), now);
                true
            }
        };

        if should_ack {
            if let Some(h) = handler {
                h.interrupt(chat_id, content);
            }
            info!(
                "Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min"
            );

            let busy_msg = format!(
                "Still processing your previous message ({elapsed_min:.0}m elapsed). \
                 Please wait for my response before sending another prompt."
            );
            let _ = adapter.send_text(chat_id, &busy_msg).await;
        }
        return;
    }

    info!(
        "Email message from {}: {}",
        chat_id,
        content.chars().take(50).collect::<String>(),
    );

    // Command detection before agent invocation
    let ctx = command_ctx(
        Platform::Email, chat_id, "dm",
        Some(chat_id.clone()), None,
        session_store, running_sessions, busy_ack_ts,
        default_model, per_chat_model, handler,
    );
    if let Some(reply) = try_handle_command(&ctx, content).await {
        let _ = adapter.send_text(chat_id, &reply).await;
        return;
    }

    {
        let mut sessions = running_sessions.lock();
        sessions.insert(chat_id.clone(), now);
    }

    let Some(handler_ref) = handler else {
        running_sessions.lock().remove(chat_id);
        warn!("No message handler registered for Email messages");
        return;
    };

    let model_override = per_chat_model.lock().get(chat_id).cloned();
    match handler_ref
        .handle_message(Platform::Email, chat_id, content, model_override.as_deref())
        .await
    {
        Ok(result) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            if result.compression_exhausted {
                let source = SessionSource {
                    platform: Platform::Email,
                    chat_id: chat_id.to_string(),
                    chat_name: Some(event.sender_name.clone()),
                    chat_type: "dm".to_string(),
                    user_id: Some(chat_id.clone()),
                    user_name: Some(event.sender_name.clone()),
                    thread_id: event.in_reply_to.clone(),
                    chat_topic: Some(event.subject.clone()),
                    user_id_alt: None,
                    chat_id_alt: None,
                };
                session_store.reset_session_for(&source);
                warn!("Session {chat_id}: compression exhausted — auto-reset performed");
                let reset_msg = "Session reset: conversation context grew too large. \
                    Starting fresh — previous context has been cleared.";
                let _ = adapter.send_text(chat_id, reset_msg).await;
            }
            if !result.response.is_empty() {
                if let Err(e) = adapter.send_text(chat_id, &result.response).await {
                    error!("Email send failed: {e}");
                }
            }
        }
        Err(e) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            error!("Agent handler failed for Email message: {e}");
            let _ = adapter
                .send_text(chat_id, "Sorry, I encountered an error processing your message.")
                .await;
        }
    }
}

/// Poll QQ Bot WebSocket event stream and route to the agent.
async fn run_qqbot(
    adapter: Arc<QqbotAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    default_model: String,
    per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    let (event_tx, mut event_rx) = mpsc::channel(64);

    let adapter_ws = adapter.clone();
    let running_ws = running.clone();
    tokio::spawn(async move {
        if let Err(e) = adapter_ws.connect_and_listen(running_ws, event_tx).await {
            error!("QQ Bot listener error: {e}");
        }
    });

    info!("QQ Bot event loop started");

    while running.load(Ordering::SeqCst) {
        match tokio::time::timeout(Duration::from_secs(1), event_rx.recv()).await {
            Ok(Some(event)) => {
                let handler_guard = handler.lock().await;
                let handler_ref = handler_guard.as_ref().cloned();
                drop(handler_guard);

                route_qqbot_message(
                    &adapter,
                    handler_ref.as_ref(),
                    &event,
                    &running_sessions,
                    &busy_ack_ts,
                    &session_store,
                    &default_model,
                    &per_chat_model,
                )
                .await;
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    info!("QQ Bot event loop stopped");
}

/// Route a QQ Bot message to the agent handler.
async fn route_qqbot_message(
    adapter: &QqbotAdapter,
    handler: Option<&Arc<dyn MessageHandler>>,
    event: &QqbotMessageEvent,
    running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: &Arc<SessionStore>,
    default_model: &str,
    per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    if event.content.is_empty() && event.media_urls.is_empty() {
        return;
    }

    let chat_id = &event.chat_id;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Check if this session is already running (busy session handling)
    let busy_elapsed_min: Option<f64> = {
        let sessions = running_sessions.lock();
        sessions.get(chat_id).map(|&start_ts| {
            let elapsed_secs = now - start_ts;
            elapsed_secs / 60.0
        })
    };

    if let Some(elapsed_min) = busy_elapsed_min {
        // Allow /stop even when the session is busy
        if GatewayCommand::parse(&event.content)
            .map(|c| c.name == "/stop")
            .unwrap_or(false)
        {
            let ctx = command_ctx(
                Platform::Qqbot,
                chat_id,
                &event.chat_type,
                Some(event.user_id.clone()),
                None,
                session_store,
                running_sessions,
                busy_ack_ts,
                default_model,
                per_chat_model,
                handler,
            );
            if let Some(reply) = try_handle_command(&ctx, &event.content).await {
                let _ = adapter.send_text(chat_id, &reply).await;
            }
            return;
        }

        let should_ack = {
            let mut ack_map = busy_ack_ts.lock();
            let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
            if now - last_ack < 30.0 {
                false
            } else {
                ack_map.insert(chat_id.to_string(), now);
                true
            }
        };

        if should_ack {
            if let Some(h) = handler {
                h.interrupt(chat_id, &event.content);
            }
            info!(
                "Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min"
            );

            let busy_msg = format!(
                "Still processing your previous message ({elapsed_min:.0}m elapsed). \
                 Please wait for my response before sending another prompt."
            );
            let _ = adapter.send_text(chat_id, &busy_msg).await;
        }
        return;
    }

    info!(
        "QQ Bot message from {} via {}: {}",
        event.user_id,
        chat_id,
        event.content.chars().take(50).collect::<String>(),
    );

    // Command detection before agent invocation
    let ctx = command_ctx(
        Platform::Qqbot,
        chat_id,
        &event.chat_type,
        Some(event.user_id.clone()),
        None,
        session_store,
        running_sessions,
        busy_ack_ts,
        default_model,
        per_chat_model,
        handler,
    );
    if let Some(reply) = try_handle_command(&ctx, &event.content).await {
        let _ = adapter.send_text(chat_id, &reply).await;
        return;
    }

    {
        let mut sessions = running_sessions.lock();
        sessions.insert(chat_id.clone(), now);
    }

    let Some(handler_ref) = handler else {
        running_sessions.lock().remove(chat_id);
        warn!("No message handler registered for QQ Bot messages");
        return;
    };

    let model_override = per_chat_model.lock().get(chat_id).cloned();
    match handler_ref
        .handle_message(Platform::Qqbot, chat_id, &event.content, model_override.as_deref())
        .await
    {
        Ok(result) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            if result.compression_exhausted {
                let source = SessionSource {
                    platform: Platform::Qqbot,
                    chat_id: chat_id.to_string(),
                    chat_name: event.user_name.clone(),
                    chat_type: event.chat_type.clone(),
                    user_id: Some(event.user_id.clone()),
                    user_name: event.user_name.clone(),
                    thread_id: None,
                    chat_topic: None,
                    user_id_alt: None,
                    chat_id_alt: None,
                };
                session_store.reset_session_for(&source);
                warn!("Session {chat_id}: compression exhausted — auto-reset performed");
                let reset_msg = "Session reset: conversation context grew too large. \
                    Starting fresh — previous context has been cleared.";
                let _ = adapter.send_text(chat_id, reset_msg).await;
            }
            if !result.response.is_empty() {
                if let Err(e) = adapter.send_text(chat_id, &result.response).await {
                    error!("QQ Bot send failed: {e}");
                }
            }
        }
        Err(e) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            error!("Agent handler failed for QQ Bot message: {e}");
            let _ = adapter
                .send_text(chat_id, "Sorry, I encountered an error processing your message.")
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_config_defaults() {
        let config = load_gateway_config();
        // Should have defaults even without config file
        assert!(!config.default_model.is_empty());
    }

    #[test]
    fn test_gateway_status() {
        let config = GatewayConfig {
            platforms: vec![],
            default_model: "test".to_string(),
        };
        let runner = GatewayRunner::new(config);
        let status = runner.status();
        assert!(!status.running);
        assert!(!status.feishu_configured);
        assert!(!status.weixin_configured);
        assert!(!status.telegram_configured);
        assert!(!status.discord_configured);
        assert!(!status.slack_configured);
        assert!(!status.api_server_configured);
        assert!(!status.dingtalk_configured);
        assert!(!status.wecom_configured);
        assert!(!status.whatsapp_configured);
        assert!(!status.qqbot_configured);
        assert!(!status.email_configured);
        assert!(!status.homeassistant_configured);
        assert!(!status.mattermost_configured);
        assert!(!status.signal_configured);
        assert!(!status.bluebubbles_configured);
        assert!(!status.wecom_callback_configured);
    }

    #[test]
    fn test_gateway_runner_platform_count() {
        let config = GatewayConfig {
            platforms: vec![
                PlatformConfigEntry {
                    platform: Platform::Whatsapp,
                    enabled: true,
                    config: PlatformConfig::default(),
                },
                PlatformConfigEntry {
                    platform: Platform::Telegram,
                    enabled: false,
                    config: PlatformConfig::default(),
                },
            ],
            default_model: "test".to_string(),
        };
        let runner = GatewayRunner::new(config);
        let status = runner.status();
        assert_eq!(status.platform_count, 1); // Only Whatsapp is enabled
    }

    #[test]
    fn test_health_check_status_platforms() {
        let running = Arc::new(AtomicBool::new(true));
        let status = HealthCheckStatus {
            running: running.clone(),
            feishu: true,
            weixin: false,
            telegram: true,
            discord: false,
            slack: true,
            api_server: false,
            dingtalk: true,
            wecom: false,
            whatsapp: true,
            webhook: false,
            qqbot: false,
            email: false,
            sms: false,
            matrix: false,
            homeassistant: false,
            mattermost: false,
            signal: false,
            bluebubbles: false,
            wecom_callback: false,
        };
        // Verify clone works since HealthCheckStatus derives Clone
        let cloned = status.clone();
        assert!(cloned.running.load(Ordering::SeqCst));
        assert!(cloned.whatsapp);
        assert!(!cloned.weixin);
    }

    #[test]
    fn test_platform_config_roundtrip() {
        // Verify that load_gateway_config falls back correctly when no config file exists
        let config = load_gateway_config();
        assert!(!config.default_model.is_empty());
    }

    // ── Command parsing tests ──────────────────────────────────────────────

    #[test]
    fn test_parse_command_simple() {
        let cmd = GatewayCommand::parse("/reset").unwrap();
        assert_eq!(cmd.name, "/reset");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn test_parse_command_with_args() {
        let cmd = GatewayCommand::parse("/model gpt-4o").unwrap();
        assert_eq!(cmd.name, "/model");
        assert_eq!(cmd.args, vec!["gpt-4o"]);
    }

    #[test]
    fn test_parse_command_multiple_args() {
        let cmd = GatewayCommand::parse("/model   anthropic/claude-sonnet   --fast").unwrap();
        assert_eq!(cmd.name, "/model");
        assert_eq!(cmd.args, vec!["anthropic/claude-sonnet", "--fast"]);
    }

    #[test]
    fn test_parse_command_case_insensitive() {
        let cmd = GatewayCommand::parse("/STATUS").unwrap();
        assert_eq!(cmd.name, "/status");
    }

    #[test]
    fn test_parse_not_command() {
        assert!(GatewayCommand::parse("hello world").is_none());
        assert!(GatewayCommand::parse("  hello  ").is_none());
    }

    #[test]
    fn test_parse_command_with_leading_whitespace() {
        let cmd = GatewayCommand::parse("  /help").unwrap();
        assert_eq!(cmd.name, "/help");
    }
}
