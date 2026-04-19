#![allow(dead_code)]
//! Home Assistant tools for controlling smart home devices via REST API.
//!
//! Mirrors the Python `tools/homeassistant_tool.py`.
//! 4 tools: ha_list_entities, ha_get_state, ha_list_services, ha_call_service.
//! Security: blocks dangerous service domains (shell_command, python_script, etc.).

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// Service domains blocked for security — allow arbitrary code/command execution.
static BLOCKED_DOMAINS: &[&str] = &[
    "shell_command",
    "command_line",
    "python_script",
    "pyscript",
    "hassio",
    "rest_command",
];

/// Regex for valid HA entity_id format (e.g. "light.living_room").
static ENTITY_ID_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z_][a-z0-9_-]*\.[a-z0-9_-]+$").unwrap());

/// Get HA config from env vars.
fn get_config() -> (String, String) {
    let url = std::env::var("HASS_URL").unwrap_or_else(|_| "http://homeassistant.local:8123".to_string());
    let url = url.trim_end_matches('/').to_string();
    let token = std::env::var("HASS_TOKEN").unwrap_or_default();
    (url, token)
}

/// Check if HA is available (HASS_TOKEN is set).
pub fn check_ha_available() -> bool {
    std::env::var("HASS_TOKEN").is_ok()
}

/// Validate entity_id format.
fn valid_entity_id(id: &str) -> bool {
    ENTITY_ID_RE.is_match(id)
}

/// Check if domain is blocked for security.
fn is_domain_blocked(domain: &str) -> bool {
    BLOCKED_DOMAINS.iter().any(|&b| b.eq_ignore_ascii_case(domain))
}

// ---------------------------------------------------------------------------
// Async API calls
// ---------------------------------------------------------------------------

async fn ha_get_states(url: &str, token: &str) -> Result<Vec<Value>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/api/states"))
        .headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("Authorization", format!("Bearer {token}").parse().unwrap());
            h.insert("Content-Type", "application/json".parse().unwrap());
            h
        })
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HA API error {}: {}", status, resp.text().await.unwrap_or_default()));
    }

    let json: Vec<Value> = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(json)
}

async fn ha_get_entity_state(url: &str, token: &str, entity_id: &str) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/api/states/{entity_id}"))
        .headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("Authorization", format!("Bearer {token}").parse().unwrap());
            h.insert("Content-Type", "application/json".parse().unwrap());
            h
        })
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HA API error {}: {}", status, resp.text().await.unwrap_or_default()));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(json)
}

async fn ha_call_service_api(
    url: &str, token: &str, domain: &str, service: &str, payload: &Value,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/api/services/{domain}/{service}"))
        .headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("Authorization", format!("Bearer {token}").parse().unwrap());
            h.insert("Content-Type", "application/json".parse().unwrap());
            h
        })
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HA API error {}: {}", status, resp.text().await.unwrap_or_default()));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(json)
}

async fn ha_get_services(url: &str, token: &str) -> Result<Vec<Value>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/api/services"))
        .headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("Authorization", format!("Bearer {token}").parse().unwrap());
            h.insert("Content-Type", "application/json".parse().unwrap());
            h
        })
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HA API error {}: {}", status, resp.text().await.unwrap_or_default()));
    }

    let json: Vec<Value> = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(json)
}

// ---------------------------------------------------------------------------
// Sync handlers
// ---------------------------------------------------------------------------

fn filter_and_summarize(states: &[Value], domain: Option<&str>, area: Option<&str>) -> Value {
    let filtered: Vec<_> = states
        .iter()
        .filter(|s| {
            if let Some(d) = domain {
                let eid = s.get("entity_id").and_then(Value::as_str).unwrap_or("");
                if !eid.starts_with(&format!("{d}.")) {
                    return false;
                }
            }
            if let Some(a) = area {
                let a_lower = a.to_lowercase();
                let attrs = s.get("attributes").and_then(Value::as_object);
                let friendly = attrs
                    .and_then(|m| m.get("friendly_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let area_val = attrs
                    .and_then(|m| m.get("area"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                if !friendly.contains(&a_lower) && !area_val.contains(&a_lower) {
                    return false;
                }
            }
            true
        })
        .map(|s| {
            serde_json::json!({
                "entity_id": s.get("entity_id").and_then(Value::as_str).unwrap_or(""),
                "state": s.get("state").and_then(Value::as_str).unwrap_or(""),
                "friendly_name": s.get("attributes").and_then(|a| a.get("friendly_name")).and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();

    serde_json::json!({
        "count": filtered.len(),
        "entities": filtered,
    })
}

/// Bridge from sync to async with graceful error handling.
fn run_async<F, T>(fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "No async runtime available".to_string())?;
    handle.block_on(fut)
}

pub fn handle_ha_list_entities(args: Value) -> Result<String, hermes_core::HermesError> {
    let (url, token) = get_config();
    if token.is_empty() {
        return Ok(tool_error("HASS_TOKEN not set. Configure Home Assistant integration."));
    }

    let domain = args.get("domain").and_then(Value::as_str);
    let area = args.get("area").and_then(Value::as_str);

    match run_async(ha_get_states(&url, &token)) {
        Ok(states) => {
            let summary = filter_and_summarize(&states, domain, area);
            Ok(serde_json::json!({ "result": summary }).to_string())
        }
        Err(e) => Ok(tool_error(format!("Failed to list entities: {e}"))),
    }
}

pub fn handle_ha_get_state(args: Value) -> Result<String, hermes_core::HermesError> {
    let (url, token) = get_config();
    if token.is_empty() {
        return Ok(tool_error("HASS_TOKEN not set. Configure Home Assistant integration."));
    }

    let entity_id = args.get("entity_id").and_then(Value::as_str).ok_or_else(|| {
        hermes_core::HermesError::new(
            hermes_core::errors::ErrorCategory::ToolError,
            "Missing required parameter: entity_id",
        )
    })?;

    if !valid_entity_id(entity_id) {
        return Ok(tool_error(format!("Invalid entity_id format: {entity_id}")));
    }

    match run_async(ha_get_entity_state(&url, &token, entity_id)) {
        Ok(data) => {
            let result = serde_json::json!({
                "entity_id": data.get("entity_id").and_then(Value::as_str).unwrap_or(""),
                "state": data.get("state").and_then(Value::as_str).unwrap_or(""),
                "attributes": data.get("attributes").cloned().unwrap_or(Value::Null),
                "last_changed": data.get("last_changed").cloned().unwrap_or(Value::Null),
                "last_updated": data.get("last_updated").cloned().unwrap_or(Value::Null),
            });
            Ok(serde_json::json!({ "result": result }).to_string())
        }
        Err(e) => Ok(tool_error(format!("Failed to get state for {entity_id}: {e}"))),
    }
}

pub fn handle_ha_call_service(args: Value) -> Result<String, hermes_core::HermesError> {
    let (url, token) = get_config();
    if token.is_empty() {
        return Ok(tool_error("HASS_TOKEN not set. Configure Home Assistant integration."));
    }

    let domain = args.get("domain").and_then(Value::as_str).unwrap_or("");
    let service = args.get("service").and_then(Value::as_str).unwrap_or("");

    if domain.is_empty() || service.is_empty() {
        return Ok(tool_error("Missing required parameters: domain and service"));
    }

    if is_domain_blocked(domain) {
        return Ok(serde_json::json!({
            "error": format!(
                "Service domain '{}' is blocked for security. Blocked domains: {}",
                domain,
                BLOCKED_DOMAINS.join(", ")
            )
        })
        .to_string());
    }

    let entity_id = args.get("entity_id").and_then(Value::as_str);
    if let Some(eid) = entity_id {
        if !valid_entity_id(eid) {
            return Ok(tool_error(format!("Invalid entity_id format: {eid}")));
        }
    }

    let mut payload = args.get("data").cloned().unwrap_or(Value::Object(Default::default()));
    if let Value::Object(ref mut map) = payload {
        if let Some(eid) = entity_id {
            map.insert("entity_id".to_string(), Value::String(eid.to_string()));
        }
    }

    match run_async(ha_call_service_api(&url, &token, domain, service, &payload)) {
        Ok(result) => {
            let affected: Vec<Value> = result
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|s| {
                            serde_json::json!({
                                "entity_id": s.get("entity_id").and_then(Value::as_str).unwrap_or(""),
                                "state": s.get("state").and_then(Value::as_str).unwrap_or(""),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let svc_result = serde_json::json!({
                "success": true,
                "service": format!("{domain}.{service}"),
                "affected_entities": affected,
            });
            Ok(serde_json::json!({ "result": svc_result }).to_string())
        }
        Err(e) => Ok(tool_error(format!("Failed to call {domain}.{service}: {e}"))),
    }
}

pub fn handle_ha_list_services(args: Value) -> Result<String, hermes_core::HermesError> {
    let (url, token) = get_config();
    if token.is_empty() {
        return Ok(tool_error("HASS_TOKEN not set. Configure Home Assistant integration."));
    }

    let domain_filter = args.get("domain").and_then(Value::as_str);

    match run_async(ha_get_services(&url, &token)) {
        Ok(services) => {
            let filtered: Vec<_> = services
                .iter()
                .filter(|s| {
                    if let Some(d) = domain_filter {
                        s.get("domain").and_then(Value::as_str) == Some(d)
                    } else {
                        true
                    }
                })
                .filter_map(|svc_domain| {
                    let d = svc_domain.get("domain")?.as_str()?.to_string();
                    let services_obj = svc_domain.get("services")?.as_object()?;
                    let mut domain_services = serde_json::Map::new();
                    for (svc_name, svc_info) in services_obj {
                        let desc = svc_info.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                        let mut entry = serde_json::Map::new();
                        entry.insert("description".to_string(), Value::String(desc));
                        if let Some(fields) = svc_info.get("fields").and_then(Value::as_object) {
                            let mut field_map = serde_json::Map::new();
                            for (k, v) in fields {
                                if let Some(desc) = v.get("description").and_then(Value::as_str) {
                                    field_map.insert(k.clone(), Value::String(desc.to_string()));
                                }
                            }
                            if !field_map.is_empty() {
                                entry.insert("fields".to_string(), Value::Object(field_map));
                            }
                        }
                        domain_services.insert(svc_name.clone(), Value::Object(entry));
                    }
                    Some(serde_json::json!({
                        "domain": d,
                        "services": domain_services,
                    }))
                })
                .collect();

            let result = serde_json::json!({
                "count": filtered.len(),
                "domains": filtered,
            });
            Ok(serde_json::json!({ "result": result }).to_string())
        }
        Err(e) => Ok(tool_error(format!("Failed to list services: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_ha_tools(registry: &mut ToolRegistry) {
    registry.register(
        "ha_list_entities".to_string(),
        "homeassistant".to_string(),
        serde_json::json!({
            "name": "ha_list_entities",
            "description": "List Home Assistant entities. Optionally filter by domain (light, switch, climate, sensor, binary_sensor, cover, fan, etc.) or by area name (living room, kitchen, bedroom, etc.).",
            "parameters": {
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "Entity domain to filter by (e.g. 'light', 'switch', 'climate', 'sensor', 'binary_sensor', 'cover', 'fan', 'media_player'). Omit to list all entities." },
                    "area": { "type": "string", "description": "Area/room name to filter by (e.g. 'living room', 'kitchen'). Matches against entity friendly names. Omit to list all." }
                },
                "required": []
            }
        }),
        std::sync::Arc::new(handle_ha_list_entities),
        Some(std::sync::Arc::new(check_ha_available)),
        vec!["HASS_TOKEN".to_string()],
        "List Home Assistant entities with optional domain/area filter".to_string(),
        "🏠".to_string(),
        None,
    );

    registry.register(
        "ha_get_state".to_string(),
        "homeassistant".to_string(),
        serde_json::json!({
            "name": "ha_get_state",
            "description": "Get the detailed state of a single Home Assistant entity, including all attributes (brightness, color, temperature setpoint, sensor readings, etc.).",
            "parameters": {
                "type": "object",
                "properties": {
                    "entity_id": { "type": "string", "description": "The entity ID to query (e.g. 'light.living_room', 'climate.thermostat', 'sensor.temperature')." }
                },
                "required": ["entity_id"]
            }
        }),
        std::sync::Arc::new(handle_ha_get_state),
        Some(std::sync::Arc::new(check_ha_available)),
        vec!["HASS_TOKEN".to_string()],
        "Get detailed state of a Home Assistant entity".to_string(),
        "🏠".to_string(),
        None,
    );

    registry.register(
        "ha_list_services".to_string(),
        "homeassistant".to_string(),
        serde_json::json!({
            "name": "ha_list_services",
            "description": "List available Home Assistant services (actions) for device control. Shows what actions can be performed on each device type and what parameters they accept. Use this to discover how to control devices found via ha_list_entities.",
            "parameters": {
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "Filter by domain (e.g. 'light', 'climate', 'switch'). Omit to list services for all domains." }
                },
                "required": []
            }
        }),
        std::sync::Arc::new(handle_ha_list_services),
        Some(std::sync::Arc::new(check_ha_available)),
        vec!["HASS_TOKEN".to_string()],
        "List available Home Assistant services for device control".to_string(),
        "🏠".to_string(),
        None,
    );

    registry.register(
        "ha_call_service".to_string(),
        "homeassistant".to_string(),
        serde_json::json!({
            "name": "ha_call_service",
            "description": "Call a Home Assistant service to control a device. Use ha_list_services to discover available services and their parameters for each domain.",
            "parameters": {
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "Service domain (e.g. 'light', 'switch', 'climate', 'cover', 'media_player', 'fan', 'scene', 'script')." },
                    "service": { "type": "string", "description": "Service name (e.g. 'turn_on', 'turn_off', 'toggle', 'set_temperature', 'set_hvac_mode', 'open_cover', 'close_cover', 'set_volume_level')." },
                    "entity_id": { "type": "string", "description": "Target entity ID (e.g. 'light.living_room'). Some services (like scene.turn_on) may not need this." },
                    "data": { "type": "object", "description": "Additional service data. Examples: {\"brightness\": 255, \"color_name\": \"blue\"} for lights, {\"temperature\": 22, \"hvac_mode\": \"heat\"} for climate, {\"volume_level\": 0.5} for media players." }
                },
                "required": ["domain", "service"]
            }
        }),
        std::sync::Arc::new(handle_ha_call_service),
        Some(std::sync::Arc::new(check_ha_available)),
        vec!["HASS_TOKEN".to_string()],
        "Call a Home Assistant service to control a device".to_string(),
        "🏠".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex to serialize tests that touch HASS_TOKEN/HASS_URL env vars.
    static ENV_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    #[test]
    fn test_valid_entity_id() {
        assert!(valid_entity_id("light.living_room"));
        assert!(valid_entity_id("sensor.temperature_1"));
        assert!(valid_entity_id("climate.thermostat"));
        assert!(valid_entity_id("switch.bedroom"));
        assert!(!valid_entity_id("INVALID"));
        assert!(!valid_entity_id("no_dot"));
        assert!(!valid_entity_id(".starts_with_dot"));
        assert!(!valid_entity_id("ends_with_dot."));
        assert!(!valid_entity_id(""));
    }

    #[test]
    fn test_blocked_domains() {
        assert!(is_domain_blocked("shell_command"));
        assert!(is_domain_blocked("python_script"));
        assert!(is_domain_blocked("hassio"));
        assert!(is_domain_blocked("rest_command"));
        assert!(is_domain_blocked("command_line"));
        assert!(is_domain_blocked("pyscript"));
        assert!(!is_domain_blocked("light"));
        assert!(!is_domain_blocked("switch"));
        assert!(!is_domain_blocked("climate"));
    }

    #[test]
    fn test_blocked_domains_case_insensitive() {
        assert!(is_domain_blocked("Shell_Command"));
        assert!(is_domain_blocked("PYTHON_SCRIPT"));
        assert!(is_domain_blocked("HassIo"));
    }

    #[test]
    fn test_valid_entity_id_with_hyphens() {
        assert!(valid_entity_id("light.my-light"));
        assert!(valid_entity_id("sensor.room-temp"));
    }

    #[test]
    fn test_check_ha_available() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("HASS_TOKEN");
        assert!(!check_ha_available());
        std::env::set_var("HASS_TOKEN", "test_token");
        assert!(check_ha_available());
        std::env::remove_var("HASS_TOKEN");
    }

    #[test]
    fn test_get_config_defaults() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("HASS_URL");
        std::env::remove_var("HASS_TOKEN");
        let (url, token) = get_config();
        assert_eq!(url, "http://homeassistant.local:8123");
        assert_eq!(token, "");
    }

    #[test]
    fn test_get_config_custom() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("HASS_URL", "http://my-ha.local:8123/");
        std::env::set_var("HASS_TOKEN", "my_token");
        let (url, token) = get_config();
        assert_eq!(url, "http://my-ha.local:8123");
        assert_eq!(token, "my_token");
        std::env::remove_var("HASS_URL");
        std::env::remove_var("HASS_TOKEN");
    }

    #[test]
    fn test_filter_by_domain() {
        let states = vec![
            serde_json::json!({"entity_id": "light.bedroom", "state": "on", "attributes": {"friendly_name": "Bedroom Light"}}),
            serde_json::json!({"entity_id": "sensor.temp", "state": "22", "attributes": {"friendly_name": "Temperature"}}),
            serde_json::json!({"entity_id": "light.kitchen", "state": "off", "attributes": {"friendly_name": "Kitchen Light"}}),
        ];

        let result = filter_and_summarize(&states, Some("light"), None);
        assert_eq!(result["count"], 2);
        assert_eq!(result["entities"][0]["entity_id"], "light.bedroom");
        assert_eq!(result["entities"][1]["entity_id"], "light.kitchen");
    }

    #[test]
    fn test_filter_by_area() {
        let states = vec![
            serde_json::json!({"entity_id": "light.bedroom", "state": "on", "attributes": {"friendly_name": "Bedroom Light"}}),
            serde_json::json!({"entity_id": "light.kitchen", "state": "off", "attributes": {"friendly_name": "Kitchen Light"}}),
        ];

        let result = filter_and_summarize(&states, None, Some("bedroom"));
        assert_eq!(result["count"], 1);
        assert_eq!(result["entities"][0]["entity_id"], "light.bedroom");
    }

    #[test]
    fn test_filter_no_match() {
        let states = vec![
            serde_json::json!({"entity_id": "light.bedroom", "state": "on", "attributes": {"friendly_name": "Bedroom Light"}}),
        ];

        let result = filter_and_summarize(&states, Some("switch"), None);
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn test_handler_list_entities_no_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("HASS_TOKEN");
        let result = handle_ha_list_entities(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_handler_get_state_missing_param() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("HASS_TOKEN", "test");
        // Missing required entity_id returns Err from the handler
        let result = handle_ha_get_state(serde_json::json!({}));
        assert!(result.is_err());
        std::env::remove_var("HASS_TOKEN");
    }

    #[test]
    fn test_handler_get_state_invalid_id() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("HASS_TOKEN", "test");
        let result = handle_ha_get_state(serde_json::json!({"entity_id": "INVALID"}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
        std::env::remove_var("HASS_TOKEN");
    }

    #[test]
    fn test_handler_call_service_blocked_domain() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("HASS_TOKEN", "test");
        let result = handle_ha_call_service(serde_json::json!({
            "domain": "shell_command",
            "service": "run",
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
        assert!(json["error"].as_str().unwrap().contains("blocked"));
        std::env::remove_var("HASS_TOKEN");
    }

    #[test]
    fn test_handler_call_service_missing_params() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("HASS_TOKEN", "test");
        let result = handle_ha_call_service(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
        std::env::remove_var("HASS_TOKEN");
    }
}
