//! Home Assistant platform adapter.
//!
//! Connects to the HA WebSocket API for real-time event monitoring.
//! State-change events are converted to agent prompts and forwarded to the
//! agent for processing.  Outbound messages are delivered as HA persistent
//! notifications via the REST API.
//!
//! Mirrors Python `gateway/platforms/homeassistant.py`.

use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::runner::MessageHandler;

// ── Constants ──────────────────────────────────────────────────────────────

const MAX_MESSAGE_LENGTH: usize = 4096;
const RECONNECT_BACKOFF: [u64; 4] = [5, 10, 30, 60];
const DEFAULT_HASS_URL: &str = "http://homeassistant.local:8123";

// ── Configuration ──────────────────────────────────────────────────────────

/// Home Assistant platform configuration.
#[derive(Debug, Clone)]
pub struct HomeAssistantConfig {
    pub hass_url: String,
    pub hass_token: String,
    pub webhook_id: String,
    pub watch_domains: Vec<String>,
    pub watch_entities: Vec<String>,
    pub ignore_entities: Vec<String>,
    pub watch_all: bool,
    pub cooldown_seconds: u64,
}

impl Default for HomeAssistantConfig {
    fn default() -> Self {
        let watch_domains = std::env::var("HASS_WATCH_DOMAINS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let watch_entities = std::env::var("HASS_WATCH_ENTITIES")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let ignore_entities = std::env::var("HASS_IGNORE_ENTITIES")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Self {
            hass_url: std::env::var("HASS_URL").unwrap_or_else(|_| DEFAULT_HASS_URL.to_string()),
            hass_token: std::env::var("HASS_TOKEN").unwrap_or_default(),
            webhook_id: std::env::var("HASS_WEBHOOK_ID").unwrap_or_default(),
            watch_domains,
            watch_entities,
            ignore_entities,
            watch_all: std::env::var("HASS_WATCH_ALL")
                .ok()
                .map(|s| matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(false),
            cooldown_seconds: std::env::var("HASS_COOLDOWN_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        }
    }
}

impl HomeAssistantConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    pub fn is_configured(&self) -> bool {
        !self.hass_token.is_empty()
    }
}

// ── Adapter ────────────────────────────────────────────────────────────────

/// Home Assistant WebSocket adapter.
pub struct HomeAssistantAdapter {
    config: HomeAssistantConfig,
    client: Client,
    last_event_time: Arc<Mutex<HashMap<String, f64>>>,
    msg_id: Arc<Mutex<i64>>,
}

impl HomeAssistantAdapter {
    pub fn new(config: HomeAssistantConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            last_event_time: Arc::new(Mutex::new(HashMap::new())),
            msg_id: Arc::new(Mutex::new(0)),
            config,
        }
    }

    async fn next_id(&self) -> i64 {
        let mut id = self.msg_id.lock().await;
        *id += 1;
        *id
    }

    // ------------------------------------------------------------------
    // Outbound messaging
    // ------------------------------------------------------------------

    /// Send a persistent notification via HA REST API.
    pub async fn send_notification(&self, content: &str) -> Result<String, String> {
        self.send_notification_raw(content, None).await
    }

    /// Send an image notification (embeds the URL as Markdown; HA frontend
    /// may render it as a clickable image link).
    pub async fn send_image(&self, image_url: &str, caption: Option<&str>) -> Result<String, String> {
        let mut msg = String::new();
        if let Some(c) = caption {
            msg.push_str(c);
            msg.push('\n');
        }
        msg.push_str(&format!("![image]({})", image_url));
        self.send_notification_raw(&msg, None).await
    }

    /// Send a document notification (embeds the URL as a Markdown link).
    pub async fn send_document(&self, doc_url: &str, caption: Option<&str>) -> Result<String, String> {
        let mut msg = String::new();
        if let Some(c) = caption {
            msg.push_str(c);
            msg.push('\n');
        }
        msg.push_str(&format!("[document]({})", doc_url));
        self.send_notification_raw(&msg, None).await
    }

    async fn send_notification_raw(
        &self,
        content: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, String> {
        let url = format!(
            "{}/api/services/persistent_notification/create",
            self.config.hass_url.trim_end_matches('/')
        );
        let mut payload = serde_json::json!({
            "title": "Hermes Agent",
            "message": content.chars().take(MAX_MESSAGE_LENGTH).collect::<String>(),
        });
        if let Some(d) = data {
            payload["data"] = d;
        }

        let resp = self
            .client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.config.hass_token),
            )
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("HA notification request failed: {e}"))?;

        if resp.status().is_success() {
            Ok(uuid::Uuid::new_v4().to_string())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "HA notification failed: HTTP {status}, body: {body}",
            ))
        }
    }

    // ------------------------------------------------------------------
    // WebSocket lifecycle
    // ------------------------------------------------------------------

    /// Run the Home Assistant WebSocket listener.
    pub async fn run(
        &self,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        shutdown_rx: oneshot::Receiver<()>,
    ) {
        let mut shutdown_rx = Some(shutdown_rx);
        let mut backoff_idx = 0;

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match self
                .connect_and_listen(&handler, &running, &mut shutdown_rx)
                .await
            {
                Ok(()) => {
                    backoff_idx = 0;
                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                }
                Err(e) => {
                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    warn!("[homeassistant] WebSocket error: {e}");
                }
            }

            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
            info!("[homeassistant] Reconnecting in {delay}s...");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
            backoff_idx = (backoff_idx + 1).min(RECONNECT_BACKOFF.len() - 1);
        }

        info!("[homeassistant] Listener stopped");
    }

    async fn connect_and_listen(
        &self,
        handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: &Arc<std::sync::atomic::AtomicBool>,
        shutdown_rx: &mut Option<oneshot::Receiver<()>>,
    ) -> Result<(), String> {
        let ws_url = self
            .config
            .hass_url
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1)
            .trim_end_matches('/')
            .to_string();
        let ws_url = format!("{ws_url}/api/websocket");

        info!("[homeassistant] Connecting to {ws_url}...");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        let (mut write_half, mut read_half) = ws_stream.split();

        // Step 1: Receive auth_required
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(10), read_half.next())
            .await
            .map_err(|_| "Timeout waiting for auth_required")?
            .ok_or("WebSocket closed before auth_required")?
            .map_err(|e| format!("WebSocket read error: {e}"))?;

        let auth_req: WsMessage = parse_ws_message(&msg)?;
        if auth_req.msg_type != "auth_required" {
            return Err(format!(
                "Expected auth_required, got: {}",
                auth_req.msg_type
            ));
        }

        // Step 2: Send auth
        let auth_msg = serde_json::json!({
            "type": "auth",
            "access_token": self.config.hass_token,
        });
        write_half
            .send(Message::Text(Utf8Bytes::from(auth_msg.to_string())))
            .await
            .map_err(|e| format!("WebSocket send auth failed: {e}"))?;

        // Step 3: Receive auth_ok
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(10), read_half.next())
            .await
            .map_err(|_| "Timeout waiting for auth_ok")?
            .ok_or("WebSocket closed before auth_ok")?
            .map_err(|e| format!("WebSocket read error: {e}"))?;

        let auth_resp: WsMessage = parse_ws_message(&msg)?;
        if auth_resp.msg_type != "auth_ok" {
            return Err(format!("Auth failed: {:?}", auth_resp));
        }

        info!("[homeassistant] Authenticated");

        // Step 4: Subscribe to state_changed events
        let sub_id = self.next_id().await;
        let sub_msg = serde_json::json!({
            "id": sub_id,
            "type": "subscribe_events",
            "event_type": "state_changed",
        });
        write_half
            .send(Message::Text(Utf8Bytes::from(sub_msg.to_string())))
            .await
            .map_err(|e| format!("WebSocket subscribe send failed: {e}"))?;

        // Verify subscription acknowledgement
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(10), read_half.next())
            .await
            .map_err(|_| "Timeout waiting for subscription ack")?
            .ok_or("WebSocket closed before subscription ack")?
            .map_err(|e| format!("WebSocket read error: {e}"))?;

        let sub_resp: ResultResponse = parse_result_response(&msg)?;
        if !sub_resp.success {
            return Err(format!("Failed to subscribe to events: {:?}", sub_resp));
        }

        info!("[homeassistant] Subscribed to state_changed events");

        // Warn if no filters configured
        if self.config.watch_domains.is_empty()
            && self.config.watch_entities.is_empty()
            && !self.config.watch_all
        {
            warn!(
                "[homeassistant] No watch_domains, watch_entities, or watch_all configured. \
                 All state_changed events will be dropped."
            );
        }

        // Optionally register webhook listener
        if !self.config.webhook_id.is_empty() {
            let webhook_sub_id = self.next_id().await;
            let webhook_msg = serde_json::json!({
                "id": webhook_sub_id,
                "type": "subscribe_trigger",
                "trigger": {
                    "platform": "webhook",
                    "webhook_id": self.config.webhook_id,
                },
            });
            if let Err(e) = write_half
                .send(Message::Text(Utf8Bytes::from(webhook_msg.to_string())))
                .await
            {
                warn!("[homeassistant] Failed to subscribe to webhook: {e}");
            }
        }

        // Listen loop
        loop {
            tokio::select! {
                msg = read_half.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                                if data.get("type").and_then(|v| v.as_str()) == Some("event") {
                                    if let Some(event) = data.get("event") {
                                        self.handle_ha_event(event, handler).await;
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("[homeassistant] WebSocket closed by server");
                            return Err("WebSocket closed".into());
                        }
                        Some(Ok(Message::Ping(_))) => {
                            debug!("[homeassistant] Ping received");
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            return Err(format!("WebSocket error: {e}"));
                        }
                        None => {
                            return Err("WebSocket stream ended".into());
                        }
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(200)) => {
                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(());
                    }
                }
                _ = async {
                    if let Some(rx) = shutdown_rx.take() {
                        let _ = rx.await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    info!("[homeassistant] Shutdown requested");
                    return Ok(());
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Event handling
    // ------------------------------------------------------------------

    async fn handle_ha_event(
        &self,
        event: &serde_json::Value,
        handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    ) {
        let event_data = event.get("data").unwrap_or(event);
        let entity_id = event_data
            .get("entity_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if entity_id.is_empty() {
            return;
        }

        // Apply ignore filter
        if self.config.ignore_entities.contains(&entity_id) {
            return;
        }

        // Apply domain/entity watch filters
        let domain = entity_id.split('.').next().unwrap_or("").to_string();
        if !self.config.watch_domains.is_empty() || !self.config.watch_entities.is_empty() {
            let domain_match = self.config.watch_domains.contains(&domain);
            let entity_match = self.config.watch_entities.contains(&entity_id);
            if !domain_match && !entity_match {
                return;
            }
        } else if !self.config.watch_all {
            return;
        }

        // Apply cooldown
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        {
            let mut last_map = self.last_event_time.lock().await;
            if let Some(&last) = last_map.get(&entity_id) {
                if (now - last) < self.config.cooldown_seconds as f64 {
                    return;
                }
            }
            last_map.insert(entity_id.clone(), now);
        }

        let old_state = event_data
            .get("old_state")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let new_state = event_data
            .get("new_state")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let message = match format_state_change(&entity_id, &old_state, &new_state) {
            Some(m) => m,
            None => return,
        };

        let handler_clone = handler.clone();
        let entity_id_clone = entity_id.clone();
        tokio::spawn(async move {
            let guard = handler_clone.lock().await;
            if let Some(h) = guard.as_ref() {
                let chat_id = format!("ha:{entity_id_clone}");
                if let Err(e) = h
                    .handle_message(Platform::Homeassistant, &chat_id, &message, None)
                    .await
                {
                    error!("[homeassistant] Agent handler failed: {e}");
                }
            } else {
                warn!("[homeassistant] No message handler registered");
            }
        });
    }
}

// ── WebSocket helpers ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WsMessage {
    #[serde(rename = "type")]
    msg_type: String,
}

#[derive(Debug, Deserialize)]
struct ResultResponse {
    #[serde(default)]
    success: bool,
}

fn parse_ws_message(msg: &Message) -> Result<WsMessage, String> {
    match msg {
        Message::Text(text) => serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}")),
        _ => Err("Expected text message".into()),
    }
}

fn parse_result_response(msg: &Message) -> Result<ResultResponse, String> {
    match msg {
        Message::Text(text) => serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}")),
        _ => Err("Expected text message".into()),
    }
}

// ── State-change formatting ────────────────────────────────────────────────

fn format_state_change(
    entity_id: &str,
    old_state: &serde_json::Value,
    new_state: &serde_json::Value,
) -> Option<String> {
    if !new_state.is_object() {
        return None;
    }

    let old_val = old_state
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let new_val = new_state
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if old_val == new_val {
        return None;
    }

    let friendly_name = new_state
        .get("attributes")
        .and_then(|a| a.get("friendly_name"))
        .and_then(|v| v.as_str())
        .unwrap_or(entity_id);

    let domain = entity_id.split('.').next().unwrap_or("");

    match domain {
        "climate" => {
            let attrs = new_state.get("attributes")?;
            let temp = attrs
                .get("current_temperature")
                .and_then(|v| v.as_f64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into());
            let target = attrs
                .get("temperature")
                .and_then(|v| v.as_f64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into());
            Some(format!(
                "[Home Assistant] {friendly_name}: HVAC mode changed from '{old_val}' to '{new_val}' (current: {temp}, target: {target})"
            ))
        }
        "sensor" => {
            let unit = new_state
                .get("attributes")
                .and_then(|a| a.get("unit_of_measurement"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(format!(
                "[Home Assistant] {friendly_name}: changed from {old_val}{unit} to {new_val}{unit}"
            ))
        }
        "binary_sensor" => Some(format!(
            "[Home Assistant] {friendly_name}: {} (was {})",
            if new_val == "on" {
                "triggered"
            } else {
                "cleared"
            },
            if old_val == "on" {
                "triggered"
            } else {
                "cleared"
            }
        )),
        "light" | "switch" | "fan" => Some(format!(
            "[Home Assistant] {friendly_name}: turned {}",
            if new_val == "on" { "on" } else { "off" }
        )),
        "alarm_control_panel" => Some(format!(
            "[Home Assistant] {friendly_name}: alarm state changed from '{old_val}' to '{new_val}'"
        )),
        _ => Some(format!(
            "[Home Assistant] {friendly_name} ({entity_id}): changed from '{old_val}' to '{new_val}'"
        )),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = HomeAssistantConfig::default();
        assert_eq!(config.hass_url, DEFAULT_HASS_URL);
        assert_eq!(config.cooldown_seconds, 30);
        assert!(!config.watch_all);
    }

    #[test]
    fn test_config_is_configured() {
        let mut config = HomeAssistantConfig::default();
        assert!(!config.is_configured());
        config.hass_token = "token".into();
        assert!(config.is_configured());
    }

    #[test]
    fn test_format_state_change_no_change() {
        let old = serde_json::json!({"state": "on"});
        let new = serde_json::json!({"state": "on"});
        assert!(format_state_change("light.living_room", &old, &new).is_none());
    }

    #[test]
    fn test_format_state_change_light() {
        let old = serde_json::json!({"state": "off"});
        let new =
            serde_json::json!({"state": "on", "attributes": {"friendly_name": "Living Room"}});
        let msg = format_state_change("light.living_room", &old, &new).unwrap();
        assert!(msg.contains("Living Room"));
        assert!(msg.contains("turned on"));
    }

    #[test]
    fn test_format_state_change_sensor() {
        let old = serde_json::json!({"state": "20.5"});
        let new = serde_json::json!({"state": "21.0", "attributes": {"friendly_name": "Temperature", "unit_of_measurement": "°C"}});
        let msg = format_state_change("sensor.temp", &old, &new).unwrap();
        assert!(msg.contains("Temperature"));
        assert!(msg.contains("°C"));
    }

    #[test]
    fn test_format_state_change_binary_sensor() {
        let old = serde_json::json!({"state": "off"});
        let new = serde_json::json!({"state": "on", "attributes": {"friendly_name": "Motion"}});
        let msg = format_state_change("binary_sensor.motion", &old, &new).unwrap();
        assert!(msg.contains("triggered"));
    }

    #[test]
    fn test_format_state_change_climate() {
        let old = serde_json::json!({"state": "off"});
        let new = serde_json::json!({
            "state": "heat",
            "attributes": {
                "friendly_name": "Thermostat",
                "current_temperature": 22.0,
                "temperature": 24.0
            }
        });
        let msg = format_state_change("climate.thermostat", &old, &new).unwrap();
        assert!(msg.contains("Thermostat"));
        assert!(msg.contains("heat"));
    }

    #[test]
    fn test_format_state_change_generic() {
        let old = serde_json::json!({"state": "idle"});
        let new = serde_json::json!({"state": "playing", "attributes": {"friendly_name": "Media Player"}});
        let msg = format_state_change("media_player.living_room", &old, &new).unwrap();
        assert!(msg.contains("Media Player"));
        assert!(msg.contains("playing"));
    }
}
