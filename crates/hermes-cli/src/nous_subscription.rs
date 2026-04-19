#![allow(dead_code)]
//! Nous subscription managed-tool capabilities.
//!
//! Mirrors Python `hermes_cli/nous_subscription.py`.
//! Detects Nous subscription status and resolves feature availability for
//! web tools, image generation, TTS, browser automation, and modal execution.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// State of a single Nous-managed feature.
#[derive(Debug, Clone, PartialEq)]
pub struct NousFeatureState {
    pub key: String,
    pub label: String,
    pub included_by_default: bool,
    pub available: bool,
    pub active: bool,
    pub managed_by_nous: bool,
    pub direct_override: bool,
    pub toolset_enabled: bool,
    pub current_provider: String,
    pub explicit_configured: bool,
}

/// Aggregated Nous subscription feature report.
#[derive(Debug, Clone)]
pub struct NousSubscriptionFeatures {
    pub subscribed: bool,
    pub nous_auth_present: bool,
    pub provider_is_nous: bool,
    features: HashMap<String, NousFeatureState>,
}

impl NousSubscriptionFeatures {
    pub fn web(&self) -> &NousFeatureState {
        self.features
            .get("web")
            .expect("web feature is always populated by get_nous_subscription_features")
    }
    pub fn image_gen(&self) -> &NousFeatureState {
        self.features
            .get("image_gen")
            .expect("image_gen feature is always populated by get_nous_subscription_features")
    }
    pub fn tts(&self) -> &NousFeatureState {
        self.features
            .get("tts")
            .expect("tts feature is always populated by get_nous_subscription_features")
    }
    pub fn browser(&self) -> &NousFeatureState {
        self.features
            .get("browser")
            .expect("browser feature is always populated by get_nous_subscription_features")
    }
    pub fn modal(&self) -> &NousFeatureState {
        self.features
            .get("modal")
            .expect("modal feature is always populated by get_nous_subscription_features")
    }

    /// Iterate features in canonical order.
    pub fn items(&self) -> Vec<&NousFeatureState> {
        let ordered = ["web", "image_gen", "tts", "browser", "modal"];
        ordered
            .iter()
            .filter_map(|k| self.features.get(*k))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

fn config_path() -> PathBuf {
    get_hermes_home().join("config.yaml")
}

fn load_config_yaml() -> serde_yaml::Value {
    let path = config_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_yaml::from_str(&content) {
                return value;
            }
        }
    }
    serde_yaml::Value::Mapping(Default::default())
}

fn get_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn model_config_dict(config: &serde_yaml::Value) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if let Some(model_cfg) = config.get("model") {
        if let Some(m) = model_cfg.as_mapping() {
            for (k, v) in m {
                if let (Some(ks), Some(vs)) = (k.as_str(), v.as_str()) {
                    result.insert(ks.to_string(), vs.to_string());
                }
            }
        } else if let Some(s) = model_cfg.as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                result.insert("default".to_string(), trimmed.to_string());
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Toolset resolution
// ---------------------------------------------------------------------------

fn _toolset_enabled(config: &serde_yaml::Value, toolset_key: &str) -> bool {
    use hermes_tools::toolsets_def::resolve_toolset;

    let default_platform_toolsets: HashMap<&str, &str> =
        [("cli", "hermes-cli")].into_iter().collect();

    let target_tools: HashSet<String> = match resolve_toolset(toolset_key) {
        Some(tools) => tools.into_iter().collect(),
        None => return false,
    };
    if target_tools.is_empty() {
        return false;
    }

    let platform_toolsets = config
        .get("platform_toolsets")
        .and_then(|v| v.as_mapping())
        .cloned();

    let platform_toolsets = match platform_toolsets {
        Some(m) if !m.is_empty() => m,
        _ => {
            let mut m = serde_yaml::Mapping::new();
            m.insert(
                serde_yaml::Value::String("cli".into()),
                serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
                    "hermes-cli".into(),
                )]),
            );
            m
        }
    };

    for (platform_val, raw_toolsets_val) in &platform_toolsets {
        let toolset_names: Vec<String> = if let Some(arr) = raw_toolsets_val.as_sequence() {
            arr.iter()
                .filter_map(|v: &serde_yaml::Value| v.as_str().map(String::from))
                .collect()
        } else {
            let platform = platform_val.as_str().unwrap_or("");
            let default = default_platform_toolsets.get(platform).copied();
            default.map(|s| vec![s.to_string()]).unwrap_or_default()
        };

        if toolset_names.is_empty() {
            let platform = platform_val.as_str().unwrap_or("");
            if let Some(default) = default_platform_toolsets.get(platform) {
                if resolve_toolset(default)
                    .map(|tools| target_tools.is_subset(&tools.into_iter().collect()))
                    .unwrap_or(false)
                {
                    return true;
                }
            }
            continue;
        }

        let mut available_tools: HashSet<String> = HashSet::new();
        for name in &toolset_names {
            if let Some(tools) = resolve_toolset(name) {
                available_tools.extend(tools);
            }
        }

        if target_tools.is_subset(&available_tools) {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Provider / backend helpers
// ---------------------------------------------------------------------------

fn _has_agent_browser() -> bool {
    which::which("agent-browser").is_ok()
}

fn _browser_label(current_provider: &str) -> String {
    match current_provider {
        "browserbase" => "Browserbase".into(),
        "browser-use" => "Browser Use".into(),
        "firecrawl" => "Firecrawl".into(),
        "camofox" => "Camofox".into(),
        "local" | "" => "Local browser".into(),
        _ => current_provider.to_string(),
    }
}

fn _tts_label(current_provider: &str) -> String {
    match current_provider {
        "openai" => "OpenAI TTS".into(),
        "elevenlabs" => "ElevenLabs".into(),
        "edge" | "" => "Edge TTS".into(),
        "mistral" => "Mistral Voxtral TTS".into(),
        "neutts" => "NeuTTS".into(),
        _ => current_provider.to_string(),
    }
}

fn normalize_browser_cloud_provider(raw: Option<&str>) -> String {
    match raw {
        None | Some("") => "local".into(),
        Some("browserbase") => "browserbase".into(),
        Some("browser-use") => "browser-use".into(),
        Some("firecrawl") => "firecrawl".into(),
        Some("camofox") => "camofox".into(),
        Some(other) => other.to_lowercase(),
    }
}

fn normalize_modal_mode(raw: Option<&str>) -> String {
    match raw {
        None | Some("") => "auto".into(),
        Some("managed") => "managed".into(),
        Some("direct") => "direct".into(),
        Some("auto") => "auto".into(),
        Some(other) => other.to_lowercase(),
    }
}

fn has_direct_modal_credentials() -> bool {
    get_env_value("MODAL_TOKEN_ID").is_some() && get_env_value("MODAL_TOKEN_SECRET").is_some()
}

#[derive(Debug, Clone)]
struct ModalBackendState {
    selected_backend: String,
}

fn resolve_modal_backend_state(
    modal_mode: &str,
    has_direct: bool,
    managed_ready: bool,
) -> ModalBackendState {
    let selected = match modal_mode {
        "managed" if managed_ready => "managed",
        "direct" if has_direct => "direct",
        "auto" if managed_ready => "managed",
        "auto" if has_direct => "direct",
        _ => "none",
    };
    ModalBackendState {
        selected_backend: selected.into(),
    }
}

// ---------------------------------------------------------------------------
// Nous auth status
// ---------------------------------------------------------------------------

fn get_nous_auth_status() -> HashMap<String, serde_yaml::Value> {
    let mut status = HashMap::new();

    let auth_store = match crate::auth_cmd::load_auth_store() {
        Ok(store) => store,
        Err(_) => {
            status.insert("logged_in".into(), serde_yaml::Value::Bool(false));
            return status;
        }
    };

    let logged_in = auth_store
        .providers
        .get("nous")
        .and_then(|n| n.access_token.as_ref())
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false);

    status.insert("logged_in".into(), serde_yaml::Value::Bool(logged_in));
    status
}

// ---------------------------------------------------------------------------
// Browser feature resolution
// ---------------------------------------------------------------------------

fn _resolve_browser_feature_state(
    browser_tool_enabled: bool,
    browser_provider: &str,
    browser_provider_explicit: bool,
    browser_local_available: bool,
    direct_camofox: bool,
    direct_browserbase: bool,
    direct_browser_use: bool,
    direct_firecrawl: bool,
    managed_browser_available: bool,
) -> (String, bool, bool, bool) {
    if direct_camofox {
        return (
            "camofox".into(),
            true,
            browser_tool_enabled,
            false,
        );
    }

    if browser_provider_explicit {
        let current_provider = if browser_provider.is_empty() {
            "local"
        } else {
            browser_provider
        };
        if current_provider == "browserbase" {
            let available = browser_local_available && direct_browserbase;
            let active = browser_tool_enabled && available;
            return (current_provider.into(), available, active, false);
        }
        if current_provider == "browser-use" {
            let provider_available = managed_browser_available || direct_browser_use;
            let available = browser_local_available && provider_available;
            let managed = browser_tool_enabled
                && browser_local_available
                && managed_browser_available
                && !direct_browser_use;
            let active = browser_tool_enabled && available;
            return (current_provider.into(), available, active, managed);
        }
        if current_provider == "firecrawl" {
            let available = browser_local_available && direct_firecrawl;
            let active = browser_tool_enabled && available;
            return (current_provider.into(), available, active, false);
        }
        if current_provider == "camofox" {
            return (current_provider.into(), false, false, false);
        }

        let available = browser_local_available;
        let active = browser_tool_enabled && available;
        return ("local".into(), available, active, false);
    }

    if managed_browser_available || direct_browser_use {
        let available = browser_local_available;
        let managed = browser_tool_enabled
            && browser_local_available
            && managed_browser_available
            && !direct_browser_use;
        let active = browser_tool_enabled && available;
        return ("browser-use".into(), available, active, managed);
    }

    if direct_browserbase {
        let available = browser_local_available;
        let active = browser_tool_enabled && available;
        return ("browserbase".into(), available, active, false);
    }

    let available = browser_local_available;
    let active = browser_tool_enabled && available;
    ("local".into(), available, active, false)
}

// ---------------------------------------------------------------------------
// Main feature detector
// ---------------------------------------------------------------------------

/// Build the full Nous subscription feature report.
pub fn get_nous_subscription_features(
    config: Option<&serde_yaml::Value>,
) -> NousSubscriptionFeatures {
    let config = config.cloned().unwrap_or_else(load_config_yaml);
    let model_cfg = model_config_dict(&config);
    let provider_is_nous = model_cfg
        .get("provider")
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default()
        == "nous";

    let nous_status = get_nous_auth_status();
    let managed_tools_flag = hermes_tools::tool_backend_helpers::managed_nous_tools_enabled();
    let nous_auth_present = nous_status
        .get("logged_in")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let subscribed = provider_is_nous || nous_auth_present;

    let web_tool_enabled = _toolset_enabled(&config, "web");
    let image_tool_enabled = _toolset_enabled(&config, "image_gen");
    let tts_tool_enabled = _toolset_enabled(&config, "tts");
    let browser_tool_enabled = _toolset_enabled(&config, "browser");
    let modal_tool_enabled = _toolset_enabled(&config, "terminal");

    let web_cfg = config
        .get("web")
        .and_then(|v| v.as_mapping())
        .cloned()
        .unwrap_or_default();
    let tts_cfg = config
        .get("tts")
        .and_then(|v| v.as_mapping())
        .cloned()
        .unwrap_or_default();
    let browser_cfg = config
        .get("browser")
        .and_then(|v| v.as_mapping())
        .cloned()
        .unwrap_or_default();
    let terminal_cfg = config
        .get("terminal")
        .and_then(|v| v.as_mapping())
        .cloned()
        .unwrap_or_default();

    let web_backend = web_cfg
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let tts_provider = tts_cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("edge")
        .trim()
        .to_lowercase();
    let browser_provider_explicit = browser_cfg.contains_key(serde_yaml::Value::String("cloud_provider".into()));
    let browser_provider = normalize_browser_cloud_provider(
        browser_cfg
            .get(serde_yaml::Value::String("cloud_provider".into()))
            .and_then(|v| v.as_str()),
    );
    let terminal_backend = terminal_cfg
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("local")
        .trim()
        .to_lowercase();
    let modal_mode = normalize_modal_mode(
        terminal_cfg
            .get(serde_yaml::Value::String("modal_mode".into()))
            .and_then(|v| v.as_str()),
    );

    let direct_exa = get_env_value("EXA_API_KEY").is_some();
    let direct_firecrawl =
        get_env_value("FIRECRAWL_API_KEY").is_some() || get_env_value("FIRECRAWL_API_URL").is_some();
    let direct_parallel = get_env_value("PARALLEL_API_KEY").is_some();
    let direct_tavily = get_env_value("TAVILY_API_KEY").is_some();
    let direct_fal = get_env_value("FAL_KEY").is_some();
    let direct_openai_tts = hermes_tools::tool_backend_helpers::resolve_openai_audio_api_key().is_some();
    let direct_elevenlabs = get_env_value("ELEVENLABS_API_KEY").is_some();
    let direct_camofox = get_env_value("CAMOFOX_URL").is_some();
    let direct_browserbase = get_env_value("BROWSERBASE_API_KEY").is_some()
        && get_env_value("BROWSERBASE_PROJECT_ID").is_some();
    let direct_browser_use = get_env_value("BROWSER_USE_API_KEY").is_some();
    let direct_modal = has_direct_modal_credentials();

    let managed_web_available = managed_tools_flag
        && nous_auth_present
        && hermes_tools::managed_tool_gateway::is_managed_tool_gateway_ready("firecrawl");
    let managed_image_available = managed_tools_flag
        && nous_auth_present
        && hermes_tools::managed_tool_gateway::is_managed_tool_gateway_ready("fal-queue");
    let managed_tts_available = managed_tools_flag
        && nous_auth_present
        && hermes_tools::managed_tool_gateway::is_managed_tool_gateway_ready("openai-audio");
    let managed_browser_available = managed_tools_flag
        && nous_auth_present
        && hermes_tools::managed_tool_gateway::is_managed_tool_gateway_ready("browser-use");
    let managed_modal_available = managed_tools_flag
        && nous_auth_present
        && hermes_tools::managed_tool_gateway::is_managed_tool_gateway_ready("modal");

    let modal_state = resolve_modal_backend_state(
        &modal_mode,
        direct_modal,
        managed_modal_available,
    );

    // Web
    let web_managed = web_backend == "firecrawl" && managed_web_available && !direct_firecrawl;
    let web_active = web_tool_enabled
        && (web_managed
            || (web_backend == "exa" && direct_exa)
            || (web_backend == "firecrawl" && direct_firecrawl)
            || (web_backend == "parallel" && direct_parallel)
            || (web_backend == "tavily" && direct_tavily));
    let web_available = managed_web_available || direct_exa || direct_firecrawl || direct_parallel || direct_tavily;

    // Image
    let image_managed = image_tool_enabled && managed_image_available && !direct_fal;
    let image_active = image_tool_enabled && (image_managed || direct_fal);
    let image_available = managed_image_available || direct_fal;

    // TTS
    let tts_current_provider = if tts_provider.is_empty() {
        "edge"
    } else {
        &tts_provider
    };
    let tts_managed = tts_tool_enabled
        && tts_current_provider == "openai"
        && managed_tts_available
        && !direct_openai_tts;
    let tts_available = tts_current_provider == "edge"
        || tts_current_provider == "neutts"
        || (tts_current_provider == "openai" && (managed_tts_available || direct_openai_tts))
        || (tts_current_provider == "elevenlabs" && direct_elevenlabs)
        || (tts_current_provider == "mistral" && get_env_value("MISTRAL_API_KEY").is_some());
    let tts_active = tts_tool_enabled && tts_available;

    // Browser
    let browser_local_available = _has_agent_browser();
    let (browser_current_provider, browser_available, browser_active, browser_managed) =
        _resolve_browser_feature_state(
            browser_tool_enabled,
            &browser_provider,
            browser_provider_explicit,
            browser_local_available,
            direct_camofox,
            direct_browserbase,
            direct_browser_use,
            direct_firecrawl,
            managed_browser_available,
        );

    // Modal
    let (modal_managed, modal_available, modal_active, modal_direct_override) =
        if terminal_backend != "modal" {
            (false, true, modal_tool_enabled, false)
        } else if modal_state.selected_backend == "managed" {
            (
                modal_tool_enabled,
                true,
                modal_tool_enabled,
                false,
            )
        } else if modal_state.selected_backend == "direct" {
            (false, true, modal_tool_enabled, modal_tool_enabled)
        } else if modal_mode == "managed" {
            (false, managed_modal_available, false, false)
        } else if modal_mode == "direct" {
            (false, direct_modal, false, false)
        } else {
            (
                false,
                managed_modal_available || direct_modal,
                false,
                false,
            )
        };

    let tts_explicit_configured = {
        let raw_tts = config.get("tts");
        if let Some(m) = raw_tts.and_then(|v| v.as_mapping()) {
            m.contains_key(serde_yaml::Value::String("provider".into()))
                && !tts_provider.is_empty()
                && tts_provider != "edge"
        } else {
            false
        }
    };

    let mut features = HashMap::new();
    features.insert(
        "web".into(),
        NousFeatureState {
            key: "web".into(),
            label: "Web tools".into(),
            included_by_default: true,
            available: web_available,
            active: web_active,
            managed_by_nous: web_managed,
            direct_override: web_active && !web_managed,
            toolset_enabled: web_tool_enabled,
            current_provider: web_backend.clone(),
            explicit_configured: !web_backend.is_empty(),
        },
    );
    features.insert(
        "image_gen".into(),
        NousFeatureState {
            key: "image_gen".into(),
            label: "Image generation".into(),
            included_by_default: true,
            available: image_available,
            active: image_active,
            managed_by_nous: image_managed,
            direct_override: image_active && !image_managed,
            toolset_enabled: image_tool_enabled,
            current_provider: if direct_fal {
                "FAL".into()
            } else if image_managed {
                "Nous Subscription".into()
            } else {
                "".into()
            },
            explicit_configured: direct_fal,
        },
    );
    features.insert(
        "tts".into(),
        NousFeatureState {
            key: "tts".into(),
            label: "OpenAI TTS".into(),
            included_by_default: true,
            available: tts_available,
            active: tts_active,
            managed_by_nous: tts_managed,
            direct_override: tts_active && !tts_managed,
            toolset_enabled: tts_tool_enabled,
            current_provider: _tts_label(tts_current_provider),
            explicit_configured: tts_explicit_configured,
        },
    );
    features.insert(
        "browser".into(),
        NousFeatureState {
            key: "browser".into(),
            label: "Browser automation".into(),
            included_by_default: true,
            available: browser_available,
            active: browser_active,
            managed_by_nous: browser_managed,
            direct_override: browser_active && !browser_managed,
            toolset_enabled: browser_tool_enabled,
            current_provider: _browser_label(&browser_current_provider),
            explicit_configured: browser_provider_explicit,
        },
    );
    features.insert(
        "modal".into(),
        NousFeatureState {
            key: "modal".into(),
            label: "Modal execution".into(),
            included_by_default: false,
            available: modal_available,
            active: modal_active,
            managed_by_nous: modal_managed,
            direct_override: terminal_backend == "modal" && modal_direct_override,
            toolset_enabled: modal_tool_enabled,
            current_provider: if terminal_backend == "modal" {
                "Modal".into()
            } else {
                terminal_backend.clone()
            },
            explicit_configured: terminal_backend == "modal",
        },
    );

    NousSubscriptionFeatures {
        subscribed,
        nous_auth_present,
        provider_is_nous,
        features,
    }
}

// ---------------------------------------------------------------------------
// Explainer lines
// ---------------------------------------------------------------------------

/// Return help text lines describing Nous subscription capabilities.
pub fn get_nous_subscription_explainer_lines() -> Vec<String> {
    if !hermes_tools::tool_backend_helpers::managed_nous_tools_enabled() {
        return Vec::new();
    }
    vec![
        "Nous subscription enables managed web tools, image generation, OpenAI TTS, and browser automation by default.".into(),
        "Those managed tools bill to your Nous subscription. Modal execution is optional and can bill to your subscription too.".into(),
        "Change these later with: hermes setup tools, hermes setup terminal, or hermes status.".into(),
    ]
}

// ---------------------------------------------------------------------------
// Default applicators
// ---------------------------------------------------------------------------

/// Apply provider-level Nous defaults shared by `hermes setup` and `hermes model`.
///
/// Returns the set of changed config keys.
pub fn apply_nous_provider_defaults(config: &mut serde_yaml::Value) -> HashSet<String> {
    if !hermes_tools::tool_backend_helpers::managed_nous_tools_enabled() {
        return HashSet::new();
    }

    let features = get_nous_subscription_features(Some(config));
    if !features.provider_is_nous {
        return HashSet::new();
    }

    let tts_cfg = config
        .get_mut("tts")
        .and_then(|v| v.as_mapping_mut());
    let tts_cfg = match tts_cfg {
        Some(m) => m,
        None => {
            // Ensure tts mapping exists
            let m = serde_yaml::Mapping::new();
            config
                .as_mapping_mut()
                .unwrap()
                .insert("tts".into(), serde_yaml::Value::Mapping(m.clone()));
            config
                .get_mut("tts")
                .unwrap()
                .as_mapping_mut()
                .unwrap()
        }
    };

    let current_tts = tts_cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("edge")
        .trim()
        .to_lowercase();
    if !current_tts.is_empty() && current_tts != "edge" {
        return HashSet::new();
    }

    tts_cfg.insert("provider".into(), serde_yaml::Value::String("openai".into()));
    let mut changed = HashSet::new();
    changed.insert("tts".into());
    changed
}

/// Apply managed Nous defaults when the user selects toolsets during setup.
///
/// Returns the set of changed config keys.
pub fn apply_nous_managed_defaults(
    config: &mut serde_yaml::Value,
    enabled_toolsets: Option<&[&str]>,
) -> HashSet<String> {
    if !hermes_tools::tool_backend_helpers::managed_nous_tools_enabled() {
        return HashSet::new();
    }

    let features = get_nous_subscription_features(Some(config));
    if !features.provider_is_nous {
        return HashSet::new();
    }

    let selected_toolsets: HashSet<String> = enabled_toolsets
        .map(|s| s.iter().map(|t| t.to_string()).collect())
        .unwrap_or_default();
    let mut changed: HashSet<String> = HashSet::new();

    let ensure_mapping = |config: &mut serde_yaml::Value, key: &str| {
        let mapping = config.as_mapping_mut().unwrap();
        if !mapping.contains_key(key) {
            mapping.insert(
                serde_yaml::Value::String(key.into()),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
        }
    };

    ensure_mapping(config, "web");
    ensure_mapping(config, "tts");
    ensure_mapping(config, "browser");

    if selected_toolsets.contains("web")
        && !features.web().explicit_configured
        && get_env_value("PARALLEL_API_KEY").is_none()
        && get_env_value("TAVILY_API_KEY").is_none()
        && get_env_value("FIRECRAWL_API_KEY").is_none()
        && get_env_value("FIRECRAWL_API_URL").is_none()
    {
        if let Some(web_cfg) = config.get_mut("web").and_then(|v| v.as_mapping_mut()) {
            web_cfg.insert("backend".into(), serde_yaml::Value::String("firecrawl".into()));
            changed.insert("web".into());
        }
    }

    if selected_toolsets.contains("tts")
        && !features.tts().explicit_configured
        && hermes_tools::tool_backend_helpers::resolve_openai_audio_api_key().is_none()
        && get_env_value("ELEVENLABS_API_KEY").is_none()
    {
        if let Some(tts_cfg) = config.get_mut("tts").and_then(|v| v.as_mapping_mut()) {
            tts_cfg.insert("provider".into(), serde_yaml::Value::String("openai".into()));
            changed.insert("tts".into());
        }
    }

    if selected_toolsets.contains("browser")
        && !features.browser().explicit_configured
        && get_env_value("BROWSER_USE_API_KEY").is_none()
        && get_env_value("BROWSERBASE_API_KEY").is_none()
    {
        if let Some(browser_cfg) = config.get_mut("browser").and_then(|v| v.as_mapping_mut()) {
            browser_cfg.insert(
                "cloud_provider".into(),
                serde_yaml::Value::String("browser-use".into()),
            );
            changed.insert("browser".into());
        }
    }

    if selected_toolsets.contains("image_gen") && get_env_value("FAL_KEY").is_none() {
        changed.insert("image_gen".into());
    }

    changed
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_config_dict_string() {
        let config: serde_yaml::Value = serde_yaml::from_str("model: nous/hermes-3").unwrap();
        let dict = model_config_dict(&config);
        assert_eq!(dict.get("default"), Some(&"nous/hermes-3".into()));
    }

    #[test]
    fn test_model_config_dict_mapping() {
        let config: serde_yaml::Value =
            serde_yaml::from_str("model:\n  provider: nous\n  default: nous/hermes-3").unwrap();
        let dict = model_config_dict(&config);
        assert_eq!(dict.get("provider"), Some(&"nous".into()));
        assert_eq!(dict.get("default"), Some(&"nous/hermes-3".into()));
    }

    #[test]
    fn test_browser_label() {
        assert_eq!(_browser_label("browserbase"), "Browserbase");
        assert_eq!(_browser_label(""), "Local browser");
        assert_eq!(_browser_label("unknown"), "unknown");
    }

    #[test]
    fn test_tts_label() {
        assert_eq!(_tts_label("openai"), "OpenAI TTS");
        assert_eq!(_tts_label(""), "Edge TTS");
        assert_eq!(_tts_label("neutts"), "NeuTTS");
    }

    #[test]
    fn test_normalize_browser_cloud_provider() {
        assert_eq!(normalize_browser_cloud_provider(Some("BrowserBase")), "browserbase");
        assert_eq!(normalize_browser_cloud_provider(None), "local");
        assert_eq!(normalize_browser_cloud_provider(Some("")), "local");
    }

    #[test]
    fn test_normalize_modal_mode() {
        assert_eq!(normalize_modal_mode(Some("MANAGED")), "managed");
        assert_eq!(normalize_modal_mode(None), "auto");
        assert_eq!(normalize_modal_mode(Some("auto")), "auto");
    }

    #[test]
    fn test_resolve_modal_backend_state() {
        let s = resolve_modal_backend_state("managed", false, true);
        assert_eq!(s.selected_backend, "managed");
        let s = resolve_modal_backend_state("direct", true, false);
        assert_eq!(s.selected_backend, "direct");
        let s = resolve_modal_backend_state("auto", true, true);
        assert_eq!(s.selected_backend, "managed");
        let s = resolve_modal_backend_state("auto", false, false);
        assert_eq!(s.selected_backend, "none");
    }

    #[test]
    fn test_toolset_enabled_empty_config() {
        let config = serde_yaml::Value::Mapping(Default::default());
        // hermes-cli toolset includes web_search + web_extract
        assert!(_toolset_enabled(&config, "web"));
    }

    /// Temporarily unset an environment variable and restore it when dropped.
    struct EnvGuard {
        key: String,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn unset(key: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key: key.to_string(),
                prev,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }

    #[test]
    fn test_get_nous_subscription_features_no_managed() {
        let _managed = EnvGuard::unset("MANAGED_NOUS_TOOLS");
        let _exa = EnvGuard::unset("EXA_API_KEY");
        let _firecrawl = EnvGuard::unset("FIRECRAWL_API_KEY");
        let _firecrawl_url = EnvGuard::unset("FIRECRAWL_API_URL");
        let _parallel = EnvGuard::unset("PARALLEL_API_KEY");
        let _tavily = EnvGuard::unset("TAVILY_API_KEY");

        let features = get_nous_subscription_features(None);
        assert!(!features.subscribed);
        assert!(!features.nous_auth_present);
        assert!(!features.provider_is_nous);
        // Web should be unavailable since no API keys and no managed mode
        assert!(!features.web().available);
    }

    #[test]
    fn test_apply_nous_provider_defaults_no_managed() {
        let _managed = EnvGuard::unset("MANAGED_NOUS_TOOLS");
        let mut config = serde_yaml::Value::Mapping(Default::default());
        let changed = apply_nous_provider_defaults(&mut config);
        assert!(changed.is_empty());
    }

    #[test]
    fn test_apply_nous_managed_defaults_no_managed() {
        let _managed = EnvGuard::unset("MANAGED_NOUS_TOOLS");
        let mut config = serde_yaml::Value::Mapping(Default::default());
        let changed = apply_nous_managed_defaults(&mut config, Some(&["web", "tts"]));
        assert!(changed.is_empty());
    }

    #[test]
    fn test_explainer_lines_when_disabled() {
        let _managed = EnvGuard::unset("MANAGED_NOUS_TOOLS");
        let lines = get_nous_subscription_explainer_lines();
        assert!(lines.is_empty());
    }

    #[test]
    fn test_nous_subscription_features_items_order() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "model:\n  provider: nous\n  default: nous/hermes-3",
        )
        .unwrap();
        let features = get_nous_subscription_features(Some(&config));
        let items = features.items();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].key, "web");
        assert_eq!(items[1].key, "image_gen");
        assert_eq!(items[2].key, "tts");
        assert_eq!(items[3].key, "browser");
        assert_eq!(items[4].key, "modal");
    }

    #[test]
    fn test_resolve_browser_feature_state_direct_camofox() {
        let (provider, available, active, managed) = _resolve_browser_feature_state(
            true, "local", false, true, true, false, false, false, false,
        );
        assert_eq!(provider, "camofox");
        assert!(available);
        assert!(active);
        assert!(!managed);
    }

    #[test]
    fn test_resolve_browser_feature_state_local_fallback() {
        let (provider, available, active, managed) = _resolve_browser_feature_state(
            true, "local", false, true, false, false, false, false, false,
        );
        assert_eq!(provider, "local");
        assert!(available);
        assert!(active);
        assert!(!managed);
    }
}
