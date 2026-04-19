//! Gateway configuration.
//!
//! Mirrors Python `gateway/config.py`: Platform enum, HomeChannel,
//! SessionResetPolicy, PlatformConfig, StreamingConfig, GatewayConfig.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Platform enum ──────────────────────────────────────────────────────────

/// Supported messaging platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    #[default]
    Local,
    Telegram,
    Discord,
    Whatsapp,
    Slack,
    Signal,
    Mattermost,
    Matrix,
    Homeassistant,
    Email,
    Sms,
    Dingtalk,
    ApiServer,
    Webhook,
    Feishu,
    Wecom,
    WecomCallback,
    Weixin,
    Bluebubbles,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Whatsapp => "whatsapp",
            Self::Slack => "slack",
            Self::Signal => "signal",
            Self::Mattermost => "mattermost",
            Self::Matrix => "matrix",
            Self::Homeassistant => "homeassistant",
            Self::Email => "email",
            Self::Sms => "sms",
            Self::Dingtalk => "dingtalk",
            Self::ApiServer => "api_server",
            Self::Webhook => "webhook",
            Self::Feishu => "feishu",
            Self::Wecom => "wecom",
            Self::WecomCallback => "wecom_callback",
            Self::Weixin => "weixin",
            Self::Bluebubbles => "bluebubbles",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Platform {
    /// Parse a platform name string into a `Platform` enum variant.
    ///
    /// Returns `None` for unknown names. Names are expected in snake_case
    /// (e.g. `"api_server"`, `"wecom_callback"`).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "telegram" => Some(Self::Telegram),
            "discord" => Some(Self::Discord),
            "whatsapp" => Some(Self::Whatsapp),
            "slack" => Some(Self::Slack),
            "signal" => Some(Self::Signal),
            "mattermost" => Some(Self::Mattermost),
            "matrix" => Some(Self::Matrix),
            "homeassistant" => Some(Self::Homeassistant),
            "email" => Some(Self::Email),
            "sms" => Some(Self::Sms),
            "dingtalk" => Some(Self::Dingtalk),
            "api_server" => Some(Self::ApiServer),
            "webhook" => Some(Self::Webhook),
            "feishu" => Some(Self::Feishu),
            "wecom" => Some(Self::Wecom),
            "wecom_callback" => Some(Self::WecomCallback),
            "weixin" => Some(Self::Weixin),
            "bluebubbles" => Some(Self::Bluebubbles),
            _ => None,
        }
    }
}

// ── HomeChannel ────────────────────────────────────────────────────────────

/// Default destination for a platform.
///
/// When a cron job specifies deliver="telegram" without a specific chat ID,
/// messages are sent to this home channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeChannel {
    pub platform: Platform,
    pub chat_id: String,
    pub name: String,
}

impl HomeChannel {
    pub fn new(platform: Platform, chat_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            platform,
            chat_id: chat_id.into(),
            name: name.into(),
        }
    }
}

impl Default for HomeChannel {
    fn default() -> Self {
        Self {
            platform: Platform::Local,
            chat_id: String::new(),
            name: "Home".to_string(),
        }
    }
}

// ── SessionResetPolicy ─────────────────────────────────────────────────────

/// Controls when sessions reset (lose context).
///
/// Modes:
/// - "daily": Reset at a specific hour each day
/// - "idle": Reset after N minutes of inactivity
/// - "both": Whichever triggers first (daily boundary OR idle timeout)
/// - "none": Never auto-reset (context managed only by compression)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionResetPolicy {
    pub mode: String,
    pub at_hour: u32,
    pub idle_minutes: u64,
    pub notify: bool,
    pub notify_exclude_platforms: Vec<String>,
}

impl Default for SessionResetPolicy {
    fn default() -> Self {
        Self {
            mode: "both".to_string(),
            at_hour: 4,
            idle_minutes: 1440,
            notify: true,
            notify_exclude_platforms: vec!["api_server".to_string(), "webhook".to_string()],
        }
    }
}

// ── PlatformConfig ─────────────────────────────────────────────────────────

/// Configuration for a single messaging platform.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformConfig {
    pub enabled: bool,
    pub token: Option<String>,
    pub api_key: Option<String>,
    pub home_channel: Option<HomeChannel>,
    /// Reply threading mode: "off", "first", "all"
    pub reply_to_mode: String,
    /// Platform-specific settings
    pub extra: HashMap<String, serde_json::Value>,
}

// ── StreamingConfig ────────────────────────────────────────────────────────

/// Configuration for real-time token streaming to messaging platforms.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamingConfig {
    pub enabled: bool,
    pub transport: String,
    pub edit_interval: f64,
    pub buffer_threshold: u32,
    pub cursor: String,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: "edit".to_string(),
            edit_interval: 1.0,
            buffer_threshold: 40,
            cursor: " \u{2589}".to_string(),
        }
    }
}

// ── GatewayConfig ──────────────────────────────────────────────────────────

/// Main gateway configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub platforms: HashMap<String, PlatformConfig>,
    pub default_reset_policy: SessionResetPolicy,
    pub reset_by_type: HashMap<String, SessionResetPolicy>,
    pub reset_by_platform: HashMap<String, SessionResetPolicy>,
    pub reset_triggers: Vec<String>,
    pub quick_commands: HashMap<String, serde_json::Value>,
    pub sessions_dir: Option<PathBuf>,
    pub always_log_local: bool,
    pub stt_enabled: bool,
    pub group_sessions_per_user: bool,
    pub thread_sessions_per_user: bool,
    pub unauthorized_dm_behavior: String,
    pub streaming: StreamingConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            platforms: HashMap::new(),
            default_reset_policy: SessionResetPolicy::default(),
            reset_by_type: HashMap::new(),
            reset_by_platform: HashMap::new(),
            reset_triggers: vec!["/new".to_string(), "/reset".to_string()],
            quick_commands: HashMap::new(),
            sessions_dir: None,
            always_log_local: true,
            stt_enabled: true,
            group_sessions_per_user: true,
            thread_sessions_per_user: false,
            unauthorized_dm_behavior: "pair".to_string(),
            streaming: StreamingConfig::default(),
        }
    }
}

impl GatewayConfig {
    /// Get the reset policy for a platform and session type.
    pub fn get_reset_policy(&self, platform: Option<Platform>, session_type: &str) -> SessionResetPolicy {
        // Check platform-specific policy first
        if let Some(p) = platform {
            let key = p.as_str().to_string();
            if let Some(policy) = self.reset_by_platform.get(&key) {
                return policy.clone();
            }
        }

        // Check type-specific policy
        if let Some(policy) = self.reset_by_type.get(session_type) {
            return policy.clone();
        }

        // Fall back to default
        self.default_reset_policy.clone()
    }

    /// Get the sessions directory, defaulting to ~/.hermes/sessions/.
    pub fn sessions_dir(&self) -> PathBuf {
        self.sessions_dir
            .clone()
            .unwrap_or_else(|| hermes_core::get_hermes_home().join("sessions"))
    }

    /// Get connected platforms (placeholder — full implementation needs
    /// platform adapter initialization).
    pub fn get_connected_platforms(&self) -> Vec<Platform> {
        self.platforms
            .iter()
            .filter_map(|(k, v)| {
                if v.enabled {
                    // Try to parse the key as a Platform
                    parse_platform(k).ok()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get the home channel for a platform.
    pub fn get_home_channel(&self, platform: Platform) -> Option<&HomeChannel> {
        self.platforms
            .get(platform.as_str())
            .and_then(|pc| pc.home_channel.as_ref())
    }
}

/// Parse a platform string into a Platform enum variant.
pub fn parse_platform(s: &str) -> Result<Platform, String> {
    match s {
        "local" => Ok(Platform::Local),
        "telegram" => Ok(Platform::Telegram),
        "discord" => Ok(Platform::Discord),
        "whatsapp" => Ok(Platform::Whatsapp),
        "slack" => Ok(Platform::Slack),
        "signal" => Ok(Platform::Signal),
        "mattermost" => Ok(Platform::Mattermost),
        "matrix" => Ok(Platform::Matrix),
        "homeassistant" => Ok(Platform::Homeassistant),
        "email" => Ok(Platform::Email),
        "sms" => Ok(Platform::Sms),
        "dingtalk" => Ok(Platform::Dingtalk),
        "api_server" => Ok(Platform::ApiServer),
        "webhook" => Ok(Platform::Webhook),
        "feishu" => Ok(Platform::Feishu),
        "wecom" => Ok(Platform::Wecom),
        "wecom_callback" => Ok(Platform::WecomCallback),
        "weixin" => Ok(Platform::Weixin),
        "bluebubbles" => Ok(Platform::Bluebubbles),
        _ => Err(format!("Unknown platform: {s}")),
    }
}

// ── Config loading ─────────────────────────────────────────────────────────

/// Coerce a value to bool, preserving the default for None.
fn coerce_bool(value: Option<&serde_json::Value>, default: bool) -> bool {
    match value {
        None => default,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().map_or(default, |v| v != 0),
        Some(serde_json::Value::String(s)) => {
            matches!(s.to_lowercase().trim(), "true" | "1" | "yes" | "on")
        }
        _ => default,
    }
}

/// Normalize unauthorized DM behavior to a supported value.
fn normalize_unauthorized_dm_behavior(value: Option<&serde_json::Value>, default: &str) -> String {
    match value {
        Some(serde_json::Value::String(s)) => {
            let s = s.trim().to_lowercase();
            if matches!(s.as_str(), "pair" | "ignore") {
                return s;
            }
            default.to_string()
        }
        _ => default.to_string(),
    }
}

/// Get an environment variable if set and non-empty.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Merge two JSON values: `patch` overwrites `base` at the top level,
/// with deep-merge for the "extra" sub-key.
fn merge_platform_data(base: &serde_json::Value, patch: &serde_json::Value) -> serde_json::Value {
    let mut result = base.clone();
    if let (Some(base_obj), Some(patch_obj)) = (result.as_object_mut(), patch.as_object()) {
        for (key, val) in patch_obj {
            if key == "extra" {
                // Deep-merge extra dicts
                let base_extra = base_obj.entry("extra".to_string())
                    .or_insert(serde_json::Value::Object(Default::default()));
                if let Some(base_extra_obj) = base_extra.as_object_mut() {
                    if let Some(patch_extra) = val.as_object() {
                        for (k, v) in patch_extra {
                            base_extra_obj.insert(k.clone(), v.clone());
                        }
                    }
                }
            } else {
                base_obj.insert(key.clone(), val.clone());
            }
        }
    }
    result
}

/// Set an env var only if not already set.
fn set_env_if_unset(name: &str, value: &str) {
    if std::env::var(name).is_err() {
        std::env::set_var(name, value);
    }
}

/// Flatten a list of strings to a comma-separated string.
fn flatten_list(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(arr) => {
            arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(",")
        }
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

/// Apply platform-specific settings from config.yaml to env vars.
fn apply_platform_env(platform_name: &str, settings: &serde_json::Value) {
    let Some(obj) = settings.as_object() else { return };

    let env_prefix = match platform_name {
        "slack" => "SLACK",
        "discord" => "DISCORD",
        "telegram" => "TELEGRAM",
        "whatsapp" => "WHATSAPP",
        "matrix" => "MATRIX",
        _ => return,
    };

    for (yaml_key, env_suffix) in &[
        ("require_mention", "REQUIRE_MENTION"),
        ("allow_bots", "ALLOW_BOTS"),
        ("reactions", "REACTIONS"),
        ("auto_thread", "AUTO_THREAD"),
        ("dm_mention_threads", "DM_MENTION_THREADS"),
    ] {
        if let Some(val) = obj.get(*yaml_key) {
            let env_name = format!("{env_prefix}_{env_suffix}");
            let val_str = match val {
                serde_json::Value::Bool(b) => b.to_string().to_lowercase(),
                serde_json::Value::Array(_) => flatten_list(val),
                _ => val.to_string(),
            };
            set_env_if_unset(&env_name, &val_str);
        }
    }

    // free_response_channels / free_response_chats / free_response_rooms
    for yaml_key in &["free_response_channels", "free_response_chats", "free_response_rooms"] {
        if let Some(val) = obj.get(*yaml_key) {
            let env_name = format!("{env_prefix}_FREE_RESPONSE_CHANNELS");
            set_env_if_unset(&env_name, &flatten_list(val));
        }
    }

    // Channel/room lists
    for (yaml_key, env_suffix) in &[
        ("ignored_channels", "IGNORED_CHANNELS"),
        ("allowed_channels", "ALLOWED_CHANNELS"),
        ("no_thread_channels", "NO_THREAD_CHANNELS"),
    ] {
        if let Some(val) = obj.get(*yaml_key) {
            let env_name = format!("{env_prefix}_{env_suffix}");
            set_env_if_unset(&env_name, &flatten_list(val));
        }
    }
}

/// Apply environment variable overrides to config.
fn apply_env_overrides(config: &mut GatewayConfig) {
    // Telegram
    if let Some(token) = env_var("TELEGRAM_BOT_TOKEN") {
        let plat = config.platforms.entry("telegram".to_string()).or_default();
        plat.enabled = true;
        plat.token = Some(token);
    }
    if let Some(mode) = env_var("TELEGRAM_REPLY_TO_MODE") {
        let mode_lower = mode.to_lowercase();
        if matches!(mode_lower.as_str(), "off" | "first" | "all") {
            config.platforms.entry("telegram".to_string()).or_default().reply_to_mode = mode_lower;
        }
    }
    if let Some(ips) = env_var("TELEGRAM_FALLBACK_IPS") {
        let plat = config.platforms.entry("telegram".to_string()).or_default();
        plat.extra.insert(
            "fallback_ips".to_string(),
            serde_json::Value::Array(
                ips.split(',').filter(|s| !s.trim().is_empty())
                    .map(|s| serde_json::Value::String(s.trim().to_string()))
                    .collect(),
            ),
        );
    }
    if let Some(chat_id) = env_var("TELEGRAM_HOME_CHANNEL") {
        if let Some(plat) = config.platforms.get_mut("telegram") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Telegram,
                &chat_id,
                env_var("TELEGRAM_HOME_CHANNEL_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }
    if let Some(mention_patterns) = env_var("TELEGRAM_MENTION_PATTERNS") {
        if let Ok(parsed) = serde_json::from_str::<Vec<String>>(&mention_patterns) {
            let plat = config.platforms.entry("telegram".to_string()).or_default();
            plat.extra.insert(
                "mention_patterns".to_string(),
                serde_json::Value::Array(parsed.into_iter().map(serde_json::Value::String).collect()),
            );
        }
    }
    if let Some(free_chats) = env_var("TELEGRAM_FREE_RESPONSE_CHATS") {
        let plat = config.platforms.entry("telegram".to_string()).or_default();
        plat.extra.insert(
            "free_response_chats".to_string(),
            serde_json::Value::String(free_chats),
        );
    }

    // Discord
    if let Some(token) = env_var("DISCORD_BOT_TOKEN") {
        let plat = config.platforms.entry("discord".to_string()).or_default();
        plat.enabled = true;
        plat.token = Some(token);
    }
    if let Some(chat_id) = env_var("DISCORD_HOME_CHANNEL") {
        if let Some(plat) = config.platforms.get_mut("discord") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Discord,
                &chat_id,
                env_var("DISCORD_HOME_CHANNEL_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }
    if let Some(mode) = env_var("DISCORD_REPLY_TO_MODE") {
        let mode_lower = mode.to_lowercase();
        if matches!(mode_lower.as_str(), "off" | "first" | "all") {
            config.platforms.entry("discord".to_string()).or_default().reply_to_mode = mode_lower;
        }
    }

    // WhatsApp
    if let Some(enabled) = env_var("WHATSAPP_ENABLED") {
        if matches!(enabled.to_lowercase().as_str(), "true" | "1" | "yes") {
            config.platforms.entry("whatsapp".to_string()).or_default().enabled = true;
        }
    }

    // Slack
    if let Some(token) = env_var("SLACK_BOT_TOKEN") {
        let plat = config.platforms.entry("slack".to_string()).or_default();
        plat.enabled = true;
        plat.token = Some(token);
    }
    if let Some(chat_id) = env_var("SLACK_HOME_CHANNEL") {
        if let Some(plat) = config.platforms.get_mut("slack") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Slack,
                &chat_id,
                env_var("SLACK_HOME_CHANNEL_NAME").unwrap_or_default(),
            ));
        }
    }

    // Signal
    if let (Some(url), Some(account)) = (env_var("SIGNAL_HTTP_URL"), env_var("SIGNAL_ACCOUNT")) {
        let plat = config.platforms.entry("signal".to_string()).or_default();
        plat.enabled = true;
        plat.extra.insert("http_url".to_string(), serde_json::Value::String(url));
        plat.extra.insert("account".to_string(), serde_json::Value::String(account));
        if let Some(ignore) = env_var("SIGNAL_IGNORE_STORIES") {
            plat.extra.insert(
                "ignore_stories".to_string(),
                serde_json::Value::Bool(matches!(ignore.to_lowercase().as_str(), "true" | "1" | "yes")),
            );
        }
    }
    if let Some(chat_id) = env_var("SIGNAL_HOME_CHANNEL") {
        if let Some(plat) = config.platforms.get_mut("signal") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Signal,
                &chat_id,
                env_var("SIGNAL_HOME_CHANNEL_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }

    // Mattermost
    if let Some(token) = env_var("MATTERMOST_TOKEN") {
        let plat = config.platforms.entry("mattermost".to_string()).or_default();
        plat.enabled = true;
        plat.token = Some(token);
        if let Some(url) = env_var("MATTERMOST_URL") {
            plat.extra.insert("url".to_string(), serde_json::Value::String(url));
        }
    }
    if let Some(chat_id) = env_var("MATTERMOST_HOME_CHANNEL") {
        if let Some(plat) = config.platforms.get_mut("mattermost") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Mattermost,
                &chat_id,
                env_var("MATTERMOST_HOME_CHANNEL_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }

    // Matrix
    let has_matrix_cred = env_var("MATRIX_ACCESS_TOKEN").is_some() || env_var("MATRIX_PASSWORD").is_some();
    if has_matrix_cred {
        let plat = config.platforms.entry("matrix".to_string()).or_default();
        plat.enabled = true;
        if let Some(t) = env_var("MATRIX_ACCESS_TOKEN") {
            plat.token = Some(t);
        }
        if let Some(homeserver) = env_var("MATRIX_HOMESERVER") {
            plat.extra.insert("homeserver".to_string(), serde_json::Value::String(homeserver));
        }
        if let Some(user_id) = env_var("MATRIX_USER_ID") {
            plat.extra.insert("user_id".to_string(), serde_json::Value::String(user_id));
        }
        if let Some(password) = env_var("MATRIX_PASSWORD") {
            plat.extra.insert("password".to_string(), serde_json::Value::String(password));
        }
        if let Some(encryption) = env_var("MATRIX_ENCRYPTION") {
            plat.extra.insert(
                "encryption".to_string(),
                serde_json::Value::Bool(matches!(encryption.to_lowercase().as_str(), "true" | "1" | "yes")),
            );
        }
        if let Some(device_id) = env_var("MATRIX_DEVICE_ID") {
            plat.extra.insert("device_id".to_string(), serde_json::Value::String(device_id));
        }
    }
    if let Some(chat_id) = env_var("MATRIX_HOME_ROOM") {
        if let Some(plat) = config.platforms.get_mut("matrix") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Matrix,
                &chat_id,
                env_var("MATRIX_HOME_ROOM_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }

    // Home Assistant
    if let Some(token) = env_var("HASS_TOKEN") {
        let plat = config.platforms.entry("homeassistant".to_string()).or_default();
        plat.enabled = true;
        plat.token = Some(token);
        if let Some(url) = env_var("HASS_URL") {
            plat.extra.insert("url".to_string(), serde_json::Value::String(url));
        }
    }

    // Email
    if let (Some(addr), Some(pwd), Some(imap), Some(smtp)) =
        (env_var("EMAIL_ADDRESS"), env_var("EMAIL_PASSWORD"), env_var("EMAIL_IMAP_HOST"), env_var("EMAIL_SMTP_HOST"))
    {
        let plat = config.platforms.entry("email".to_string()).or_default();
        plat.enabled = true;
        plat.extra.insert("address".to_string(), serde_json::Value::String(addr));
        plat.extra.insert("imap_host".to_string(), serde_json::Value::String(imap));
        plat.extra.insert("smtp_host".to_string(), serde_json::Value::String(smtp));
        plat.extra.insert("password".to_string(), serde_json::Value::String(pwd));
    }
    if let Some(chat_id) = env_var("EMAIL_HOME_ADDRESS") {
        if let Some(plat) = config.platforms.get_mut("email") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Email,
                &chat_id,
                env_var("EMAIL_HOME_ADDRESS_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }

    // SMS (Twilio)
    if let Some(sid) = env_var("TWILIO_ACCOUNT_SID") {
        let plat = config.platforms.entry("sms".to_string()).or_default();
        plat.enabled = true;
        plat.api_key = env_var("TWILIO_AUTH_TOKEN");
        plat.extra.insert("account_sid".to_string(), serde_json::Value::String(sid));
    }
    if let Some(chat_id) = env_var("SMS_HOME_CHANNEL") {
        if let Some(plat) = config.platforms.get_mut("sms") {
            plat.home_channel = Some(HomeChannel::new(
                Platform::Sms,
                &chat_id,
                env_var("SMS_HOME_CHANNEL_NAME").unwrap_or_else(|| "Home".to_string()),
            ));
        }
    }

    // Dingtalk
    if let Some(token) = env_var("DINGTALK_BOT_TOKEN") {
        let plat = config.platforms.entry("dingtalk".to_string()).or_default();
        plat.enabled = true;
        plat.token = Some(token);
    }

    // Feishu
    if let Some(app_id) = env_var("FEISHU_APP_ID") {
        let plat = config.platforms.entry("feishu".to_string()).or_default();
        plat.enabled = true;
        plat.extra.insert("app_id".to_string(), serde_json::Value::String(app_id));
        if let Some(secret) = env_var("FEISHU_APP_SECRET") {
            plat.extra.insert("app_secret".to_string(), serde_json::Value::String(secret));
        }
    }

    // Weixin
    if let Some(token) = env_var("WEIXIN_TOKEN") {
        let plat = config.platforms.entry("weixin".to_string()).or_default();
        plat.extra.insert("token".to_string(), serde_json::Value::String(token.clone()));
        plat.token = Some(token);
    }
    if let Some(account_id) = env_var("WEIXIN_ACCOUNT_ID") {
        config.platforms.entry("weixin".to_string()).or_default()
            .extra.insert("account_id".to_string(), serde_json::Value::String(account_id));
    }

    // BlueBubbles
    if let (Some(url), Some(password)) = (env_var("BLUEBUBBLES_SERVER_URL"), env_var("BLUEBUBBLES_PASSWORD")) {
        let plat = config.platforms.entry("bluebubbles".to_string()).or_default();
        plat.enabled = true;
        plat.extra.insert("server_url".to_string(), serde_json::Value::String(url));
        plat.extra.insert("password".to_string(), serde_json::Value::String(password));
    }
}

/// Load gateway configuration from multiple sources.
///
/// Priority (highest to lowest):
/// 1. Environment variables
/// 2. ~/.hermes/config.yaml (primary user-facing config)
/// 3. ~/.hermes/gateway.json (legacy)
/// 4. Built-in defaults
pub fn load_gateway_config() -> GatewayConfig {
    let home = hermes_core::get_hermes_home();
    let mut gw_data = serde_json::Map::new();

    // Legacy fallback: gateway.json
    let gateway_json_path = home.join("gateway.json");
    if gateway_json_path.exists() {
        if let Ok(data) = std::fs::read_to_string(&gateway_json_path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(obj) = parsed.as_object() {
                    gw_data = obj.clone();
                }
            }
        }
    }

    // Primary source: config.yaml
    let config_yaml_path = home.join("config.yaml");
    if config_yaml_path.exists() {
        if let Ok(data) = std::fs::read_to_string(&config_yaml_path) {
            if let Ok(yaml_cfg) = serde_yaml::from_str::<serde_json::Value>(&data) {
                if let Some(yaml_obj) = yaml_cfg.as_object() {
                    // Map config.yaml keys → GatewayConfig schema
                    if let Some(sr) = yaml_obj.get("session_reset") {
                        if sr.is_object() {
                            gw_data.insert("default_reset_policy".to_string(), sr.clone());
                        }
                    }
                    if let Some(qc) = yaml_obj.get("quick_commands") {
                        if qc.is_object() {
                            gw_data.insert("quick_commands".to_string(), qc.clone());
                        }
                    }
                    if let Some(stt) = yaml_obj.get("stt") {
                        if stt.is_object() {
                            gw_data.insert("stt".to_string(), stt.clone());
                        }
                    }
                    for key in &[
                        "group_sessions_per_user",
                        "thread_sessions_per_user",
                        "streaming",
                        "reset_triggers",
                        "always_log_local",
                    ] {
                        if let Some(val) = yaml_obj.get(*key) {
                            gw_data.insert((*key).to_string(), val.clone());
                        }
                    }
                    if let Some(val) = yaml_obj.get("unauthorized_dm_behavior") {
                        gw_data.insert(
                            "unauthorized_dm_behavior".to_string(),
                            serde_json::Value::String(normalize_unauthorized_dm_behavior(
                                Some(val),
                                "pair",
                            )),
                        );
                    }

                    // Merge platforms
                    if let Some(platforms_obj) = gw_data
                        .entry("platforms".to_string())
                        .or_insert(serde_json::Value::Object(Default::default()))
                        .as_object_mut()
                    {
                        if let Some(yaml_platforms) = yaml_obj.get("platforms").and_then(|v| v.as_object()) {
                            for (plat_name, plat_block) in yaml_platforms {
                                let existing = platforms_obj.get(plat_name).cloned()
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                                let merged = merge_platform_data(&existing, plat_block);
                                platforms_obj.insert(plat_name.clone(), merged);
                            }
                        }
                    }

                    // Apply platform-specific env var mappings
                    for plat_name in &[
                        "slack", "discord", "telegram", "whatsapp", "matrix",
                    ] {
                        if let Some(plat_settings) = yaml_obj.get(*plat_name).and_then(|v| v.as_object()) {
                            apply_platform_env(plat_name, &serde_json::Value::Object(plat_settings.clone()));
                        }
                    }
                }
            }
        }
    }

    let mut config = GatewayConfig::from_dict(&serde_json::Value::Object(gw_data));

    // Apply environment variable overrides
    apply_env_overrides(&mut config);

    // Validate
    let policy = &mut config.default_reset_policy;
    if !(0..=23).contains(&policy.at_hour) {
        policy.at_hour = 4;
    }
    if policy.idle_minutes == 0 {
        policy.idle_minutes = 1440;
    }

    config
}

/// Save gateway configuration to config.yaml.
pub fn save_gateway_config(config: &GatewayConfig, path: Option<&std::path::Path>) -> Result<(), String> {
    let target = path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| hermes_core::get_hermes_home().join("config.yaml"));

    let yaml = serde_yaml::to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&target, yaml).map_err(|e| e.to_string())
}

/// Convert a JSON Value into the appropriate type for GatewayConfig fields.
impl GatewayConfig {
    /// Create a GatewayConfig from a JSON value (deserialized YAML).
    pub fn from_dict(data: &serde_json::Value) -> Self {
        let obj = data.as_object().cloned().unwrap_or_default();

        // Platforms
        let mut platforms = HashMap::new();
        if let Some(platforms_data) = obj.get("platforms").and_then(|v| v.as_object()) {
            for (platform_name, platform_data) in platforms_data {
                if let Ok(platform) = parse_platform(platform_name) {
                    let plat_config = PlatformConfig::from_dict(platform_data);
                    platforms.insert(platform.as_str().to_string(), plat_config);
                }
            }
        }

        // Reset policies
        let default_reset_policy = obj.get("default_reset_policy")
            .map(SessionResetPolicy::from_dict)
            .unwrap_or_default();

        let mut reset_by_type = HashMap::new();
        if let Some(rbt) = obj.get("reset_by_type").and_then(|v| v.as_object()) {
            for (type_name, policy_data) in rbt {
                reset_by_type.insert(type_name.clone(), SessionResetPolicy::from_dict(policy_data));
            }
        }

        let mut reset_by_platform = HashMap::new();
        if let Some(rbp) = obj.get("reset_by_platform").and_then(|v| v.as_object()) {
            for (platform_name, policy_data) in rbp {
                if let Ok(platform) = parse_platform(platform_name) {
                    reset_by_platform.insert(platform.as_str().to_string(), SessionResetPolicy::from_dict(policy_data));
                }
            }
        }

        // Reset triggers
        let reset_triggers = obj.get("reset_triggers")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_else(|| vec!["/new".to_string(), "/reset".to_string()]);

        // Quick commands
        let quick_commands = obj.get("quick_commands")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        // Sessions dir
        let sessions_dir = obj.get("sessions_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        // Bool fields
        let always_log_local = coerce_bool(obj.get("always_log_local"), true);
        let stt_enabled = coerce_bool(obj.get("stt_enabled"), true);
        let group_sessions_per_user = coerce_bool(obj.get("group_sessions_per_user"), true);
        let thread_sessions_per_user = coerce_bool(obj.get("thread_sessions_per_user"), false);

        // Unauthorized DM behavior
        let unauthorized_dm_behavior = normalize_unauthorized_dm_behavior(
            obj.get("unauthorized_dm_behavior"),
            "pair",
        );

        // Streaming
        let streaming = obj.get("streaming")
            .map(StreamingConfig::from_dict)
            .unwrap_or_default();

        Self {
            platforms,
            default_reset_policy,
            reset_by_type,
            reset_by_platform,
            reset_triggers,
            quick_commands,
            sessions_dir,
            always_log_local,
            stt_enabled,
            group_sessions_per_user,
            thread_sessions_per_user,
            unauthorized_dm_behavior,
            streaming,
        }
    }
}

impl HomeChannel {
    pub fn from_dict(data: &serde_json::Value) -> Self {
        let obj = data.as_object().cloned().unwrap_or_default();
        let platform = obj.get("platform")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_platform(s).ok())
            .unwrap_or_default();
        Self {
            platform,
            chat_id: obj.get("chat_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            name: obj.get("name").and_then(|v| v.as_str()).unwrap_or("Home").to_string(),
        }
    }
}

impl PlatformConfig {
    pub fn from_dict(data: &serde_json::Value) -> Self {
        let obj = data.as_object().cloned().unwrap_or_default();
        let enabled = coerce_bool(obj.get("enabled"), false);
        let token = obj.get("token").and_then(|v| v.as_str()).map(String::from);
        let api_key = obj.get("api_key").and_then(|v| v.as_str()).map(String::from);
        let home_channel = obj.get("home_channel").map(HomeChannel::from_dict);
        let reply_to_mode = obj.get("reply_to_mode")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| "first".to_string());
        let extra = obj.get("extra")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        Self {
            enabled,
            token,
            api_key,
            home_channel,
            reply_to_mode,
            extra,
        }
    }
}

impl SessionResetPolicy {
    pub fn from_dict(data: &serde_json::Value) -> Self {
        let obj = data.as_object().cloned().unwrap_or_default();
        let mode = obj.get("mode")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| "both".to_string());
        let at_hour = obj.get("at_hour")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(4);
        let idle_minutes = obj.get("idle_minutes")
            .and_then(|v| v.as_u64())
            .unwrap_or(1440);
        let notify = coerce_bool(obj.get("notify"), true);
        let notify_exclude_platforms = obj.get("notify_exclude_platforms")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_else(|| vec!["api_server".to_string(), "webhook".to_string()]);
        Self {
            mode,
            at_hour,
            idle_minutes,
            notify,
            notify_exclude_platforms,
        }
    }
}

impl StreamingConfig {
    pub fn from_dict(data: &serde_json::Value) -> Self {
        let obj = data.as_object().cloned().unwrap_or_default();
        let enabled = coerce_bool(obj.get("enabled"), false);
        let transport = obj.get("transport")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| "edit".to_string());
        let edit_interval = obj.get("edit_interval")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);
        let buffer_threshold = obj.get("buffer_threshold")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(40);
        let cursor = obj.get("cursor")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| " \u{2589}".to_string());
        Self {
            enabled,
            transport,
            edit_interval,
            buffer_threshold,
            cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_roundtrip() {
        for p in [
            Platform::Local,
            Platform::Telegram,
            Platform::Discord,
            Platform::Weixin,
            Platform::Feishu,
        ] {
            assert_eq!(parse_platform(p.as_str()).unwrap(), p);
        }
    }

    #[test]
    fn test_parse_platform_unknown() {
        assert!(parse_platform("unknown_platform").is_err());
    }

    #[test]
    fn test_default_reset_policy() {
        let policy = SessionResetPolicy::default();
        assert_eq!(policy.mode, "both");
        assert_eq!(policy.at_hour, 4);
        assert_eq!(policy.idle_minutes, 1440);
        assert!(policy.notify);
    }

    #[test]
    fn test_gateway_config_defaults() {
        let config = GatewayConfig::default();
        assert!(config.platforms.is_empty());
        assert_eq!(config.default_reset_policy.mode, "both");
        assert!(config.group_sessions_per_user);
        assert!(!config.thread_sessions_per_user);
    }

    #[test]
    fn test_sessions_dir_default() {
        let config = GatewayConfig::default();
        let dir = config.sessions_dir();
        assert!(dir.to_string_lossy().contains("sessions"));
    }

    #[test]
    fn test_get_reset_policy_default() {
        let config = GatewayConfig::default();
        let policy = config.get_reset_policy(Some(Platform::Telegram), "dm");
        assert_eq!(policy.mode, "both");
    }

    #[test]
    fn test_get_reset_policy_by_platform() {
        let mut config = GatewayConfig::default();
        config.reset_by_platform.insert(
            "telegram".to_string(),
            SessionResetPolicy {
                mode: "idle".to_string(),
                idle_minutes: 30,
                ..Default::default()
            },
        );
        let policy = config.get_reset_policy(Some(Platform::Telegram), "dm");
        assert_eq!(policy.mode, "idle");
        assert_eq!(policy.idle_minutes, 30);
    }

    #[test]
    fn test_config_from_dict_empty() {
        let config = GatewayConfig::from_dict(&serde_json::Value::Object(Default::default()));
        assert_eq!(config.reset_triggers, vec!["/new", "/reset"]);
        assert!(config.platforms.is_empty());
    }

    #[test]
    fn test_config_from_dict_with_platform() {
        let data = serde_json::json!({
            "platforms": {
                "telegram": {
                    "enabled": true,
                    "token": "test_token_123",
                    "reply_to_mode": "all"
                }
            },
            "reset_triggers": ["/new", "/reset", "/clear"],
            "group_sessions_per_user": false,
            "streaming": {
                "enabled": true,
                "edit_interval": 0.5
            }
        });
        let config = GatewayConfig::from_dict(&data);
        assert!(config.platforms.contains_key("telegram"));
        let telegram = config.platforms.get("telegram").unwrap();
        assert!(telegram.enabled);
        assert_eq!(telegram.token.as_deref(), Some("test_token_123"));
        assert_eq!(telegram.reply_to_mode, "all");
        assert_eq!(config.reset_triggers, vec!["/new", "/reset", "/clear"]);
        assert!(!config.group_sessions_per_user);
        assert!(config.streaming.enabled);
        assert_eq!(config.streaming.edit_interval, 0.5);
    }

    #[test]
    fn test_config_from_dict_reset_policies() {
        let data = serde_json::json!({
            "default_reset_policy": {
                "mode": "idle",
                "idle_minutes": 60
            },
            "reset_by_type": {
                "group": {
                    "mode": "daily",
                    "at_hour": 6
                }
            }
        });
        let config = GatewayConfig::from_dict(&data);
        assert_eq!(config.default_reset_policy.mode, "idle");
        assert_eq!(config.default_reset_policy.idle_minutes, 60);
        assert_eq!(config.reset_by_type.get("group").map(|p| &p.mode), Some(&"daily".to_string()));
    }

    #[test]
    fn test_coerce_bool_variants() {
        assert!(coerce_bool(Some(&serde_json::Value::Bool(true)), false));
        assert!(!coerce_bool(Some(&serde_json::Value::Bool(false)), true));
        assert!(coerce_bool(Some(&serde_json::Value::String("true".to_string())), false));
        assert!(coerce_bool(Some(&serde_json::Value::String("yes".to_string())), false));
        assert!(coerce_bool(Some(&serde_json::Value::String("1".to_string())), false));
        assert!(!coerce_bool(Some(&serde_json::Value::String("false".to_string())), true));
        assert!(!coerce_bool(Some(&serde_json::Value::String("no".to_string())), true));
        assert!(coerce_bool(None, true));
        assert!(!coerce_bool(None, false));
    }

    #[test]
    fn test_normalize_unauthorized_dm_behavior() {
        assert_eq!(normalize_unauthorized_dm_behavior(
            Some(&serde_json::Value::String("pair".to_string())),
            "ignore",
        ), "pair");
        assert_eq!(normalize_unauthorized_dm_behavior(
            Some(&serde_json::Value::String("INVALID".to_string())),
            "pair",
        ), "pair");
        assert_eq!(normalize_unauthorized_dm_behavior(None, "ignore"), "ignore");
    }

    #[test]
    fn test_streaming_config_from_dict() {
        let data = serde_json::json!({
            "enabled": true,
            "transport": "off",
            "buffer_threshold": 100
        });
        let cfg = StreamingConfig::from_dict(&data);
        assert!(cfg.enabled);
        assert_eq!(cfg.transport, "off");
        assert_eq!(cfg.buffer_threshold, 100);
    }

    #[test]
    fn test_home_channel_from_dict() {
        let data = serde_json::json!({
            "platform": "telegram",
            "chat_id": "12345",
            "name": "My Home"
        });
        let hc = HomeChannel::from_dict(&data);
        assert_eq!(hc.platform, Platform::Telegram);
        assert_eq!(hc.chat_id, "12345");
        assert_eq!(hc.name, "My Home");
    }

    #[test]
    fn test_default_matches_from_dict_empty() {
        let default_config = GatewayConfig::default();
        let parsed_config = GatewayConfig::from_dict(&serde_json::Value::Object(Default::default()));

        // Core defaults must match
        assert_eq!(default_config.unauthorized_dm_behavior, parsed_config.unauthorized_dm_behavior);
        assert_eq!(default_config.reset_triggers, parsed_config.reset_triggers);
        assert_eq!(default_config.always_log_local, parsed_config.always_log_local);
        assert_eq!(default_config.stt_enabled, parsed_config.stt_enabled);
        assert_eq!(default_config.group_sessions_per_user, parsed_config.group_sessions_per_user);
        assert_eq!(default_config.thread_sessions_per_user, parsed_config.thread_sessions_per_user);
    }
}
