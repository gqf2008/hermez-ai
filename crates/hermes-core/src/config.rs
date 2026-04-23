//! Hermes configuration types.
//!
//! Mirrors the Python `hermes_cli/config.py` DEFAULT_CONFIG and OPTIONAL_ENV_VARS.
//! All configuration is loaded from YAML config + environment variables with
//! env vars taking precedence.
//!
//! ## Config Version Migration
//!
//! Config files are tracked via `_config_version` (current: 18, matching Python).
//! On load, older configs are automatically migrated through each version step.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::errors::{ErrorCategory, HermesError, Result};
use crate::hermes_home::get_hermes_home;

/// Current config schema version.
/// Kept at 18 to stay aligned with Python `hermes_cli/config.py`.
/// Rust-specific additions (credential pool, provider prefs, fallbacks, memory)
/// are added without bumping the version number so that configs remain
/// interchangeable between Python and Rust implementations.
const LATEST_CONFIG_VERSION: u32 = 18;

/// Custom deserializer for context_length that accepts both integers and
/// string values like "256K". Emits a warning for non-integer values,
/// mirroring Python PR 93fe4ead.
fn deserialize_context_length<'de, D>(deserializer: D) -> std::result::Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)
        .map_err(|e| <D::Error as serde::de::Error>::custom(e.to_string()))?;
    let Some(value) = value else {
        return Ok(None);
    };

    // Try integer first
    if let Some(n) = value.as_u64() {
        return Ok(Some(n as usize));
    }

    // Try string — warn if it looks like a suffixed value
    if let Some(s) = value.as_str() {
        // Try plain numeric string
        if let Ok(n) = s.parse::<usize>() {
            return Ok(Some(n));
        }

        // Looks like "256K", "128k", "1M", etc. — warn and fall through
        tracing::warn!(
            "Invalid model.context_length in config.yaml: {:?} — \
             must be a plain integer (e.g. 256000, not '256K'). \
             Falling back to auto-detection.",
            s
        );
        eprintln!(
            "\n\u{26A0} Invalid model.context_length in config.yaml: {:?}\n \
             Must be a plain integer (e.g. 256000, not '256K').\n \
             Falling back to auto-detected context window.\n",
            s
        );
    }

    // Null or other type — return None (auto-detect)
    Ok(None)
}

/// Main configuration structure.
///
/// Mirrors the `~/.hermes/config.yaml` schema. Fields are optional to support
/// partial configs where missing values fall back to defaults or env vars.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HermesConfig {
    /// LLM model configuration
    pub model: ModelConfig,
    /// Named custom providers (e.g., {openrouter: {base_url: ..., api_key: ...}})
    pub providers: HashMap<String, CustomProviderConfig>,
    /// Credential pool strategies for failover/rotation
    pub credential_pool_strategies: HashMap<String, CredentialPoolStrategyConfig>,
    /// Ordered fallback provider chain
    pub fallback_providers: Vec<String>,
    /// Provider preferences for OpenRouter
    pub provider: Option<ProviderPreferencesConfig>,
    /// Terminal execution configuration
    pub terminal: TerminalConfig,
    /// File operation configuration
    pub file: FileConfig,
    /// Tool approval settings
    pub approvals: ApprovalConfig,
    /// Skills configuration
    pub skills: SkillsConfig,
    /// Memory configuration
    pub memory: MemoryConfig,
    /// Context compression configuration
    pub compression: CompressionConfig,
    /// MCP server configuration
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Cron job configuration
    pub cron: CronConfig,
    /// Browser tool configuration
    pub browser: BrowserConfig,
    /// Auxiliary model configuration
    pub auxiliary_model: AuxiliaryModelConfig,
    /// Security settings
    pub security: SecurityConfig,
    /// Skin / theme settings
    pub skin: Option<String>,
    /// Disabled tools (global)
    pub disabled_tools: Vec<String>,
    /// Disabled toolsets (global)
    pub disabled_toolsets: Vec<String>,
    /// Platform-specific disabled skills
    pub skills_platform_disabled: HashMap<String, Vec<String>>,
    /// Plugin configuration
    pub plugins: PluginsConfig,
    /// Config version for migration (current: 18)
    #[serde(rename = "_config_version", skip_serializing_if = "Option::is_none")]
    pub config_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Primary model name (e.g., "anthropic/claude-opus-4-6")
    pub name: Option<String>,
    /// Provider override (e.g., "openrouter", "anthropic", "openai")
    pub provider: Option<String>,
    /// Base URL for custom endpoints
    pub base_url: Option<String>,
    /// API key (also read from env: OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.)
    pub api_key: Option<String>,
    /// API mode: "openai", "anthropic_messages", "codex_responses"
    pub api_mode: Option<String>,
    /// Context length override
    #[serde(deserialize_with = "deserialize_context_length")]
    pub context_length: Option<usize>,
    /// Temperature
    pub temperature: Option<f64>,
    /// Max tokens
    pub max_tokens: Option<usize>,
    /// Reasoning effort: "low", "medium", "high"
    pub reasoning_effort: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            name: Some("anthropic/claude-opus-4-6".to_string()),
            provider: None,
            base_url: None,
            api_key: None,
            api_mode: Some("anthropic_messages".to_string()),
            context_length: None,
            temperature: Some(0.7),
            max_tokens: None,
            reasoning_effort: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Terminal backend: "local", "docker", "ssh", "modal", "singularity", "daytona"
    pub backend: String,
    /// Working directory for terminal sessions
    pub cwd: Option<PathBuf>,
    /// Sudo password for sudo -S (also from HERMES_SUDO_PASSWORD env)
    pub sudo_password: Option<String>,
    /// Max output size in characters
    pub max_output_size: usize,
    /// Sandbox lifetime in seconds
    pub lifetime_seconds: u64,
    /// Docker image to use
    pub docker_image: Option<String>,
    /// SSH host (for ssh backend)
    pub ssh_host: Option<String>,
    /// SSH user (for ssh backend)
    pub ssh_user: Option<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            backend: "local".to_string(),
            cwd: None,
            sudo_password: None,
            max_output_size: 100_000,
            lifetime_seconds: 3600,
            docker_image: None,
            ssh_host: None,
            ssh_user: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// Max read size in characters
    pub max_read_size: usize,
    /// Consecutive re-read limit before hard block
    pub max_consecutive_reads: usize,
    /// Sensitive paths that should be protected
    pub sensitive_paths: Vec<String>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            max_read_size: 100_000,
            max_consecutive_reads: 4,
            sensitive_paths: vec![
                "/etc/".to_string(),
                "/boot/".to_string(),
                "/var/run/docker.sock".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    /// Mode: "off", "smart", "strict"
    pub mode: String,
    /// Permanent allowlist of commands
    pub permanent_allowlist: Vec<String>,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            mode: "smart".to_string(),
            permanent_allowlist: vec![],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Globally disabled skills
    pub disabled: Vec<String>,
    /// Platform-specific disabled skills (legacy, per-platform key)
    pub platform_disabled: HashMap<String, Vec<String>>,
    /// External skill directories
    pub external_dirs: Vec<PathBuf>,
}

/// Plugin configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Globally disabled plugins
    pub disabled: Vec<String>,
    /// Auto-load plugins on startup
    pub auto_load: bool,
    /// Plugin search directories
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Memory backend: "honcho", "holographic", "mem0", "retaindb", etc.
    pub backend: Option<String>,
    /// Whether memory is enabled
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Whether context compression is enabled
    pub enabled: bool,
    /// Target token count for compression
    pub target_tokens: Option<usize>,
    /// Summarization model override
    pub model: Option<String>,
    /// Number of first messages to protect
    pub protect_first_n: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// For stdio transport
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    /// For HTTP/StreamableHTTP transport
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    /// Timeout in seconds
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CronConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub provider: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuxiliaryModelConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Per-task auxiliary model overrides (e.g., "summarize", "vision", "search")
    pub tasks: HashMap<String, AuxiliaryTaskConfig>,
}

/// Per-task auxiliary model configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuxiliaryTaskConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Request timeout in seconds.
    pub timeout: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Whether to enable OSV vulnerability checking
    pub osv_check: bool,
    /// Website access policy rules path
    pub website_policy_rules: Option<PathBuf>,
}

/// Named custom provider configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_mode: Option<String>,
    /// Alternative URL field (alias for base_url).
    pub url: Option<String>,
    /// API endpoint field (alias for base_url).
    pub api: Option<String>,
    /// Human-readable display name.
    pub name: Option<String>,
    /// Default model for this provider.
    pub default_model: Option<String>,
    /// Env var name containing the API key.
    pub key_env: Option<String>,
}

/// Credential pool strategy for a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CredentialPoolStrategyConfig {
    /// List of credential entries with api_key, base_url, label
    pub credentials: Vec<serde_json::Value>,
    /// Rotation mode: "round_robin", "failover"
    pub mode: Option<String>,
}

/// Provider preferences sent only to OpenRouter endpoints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderPreferencesConfig {
    /// Only use these providers
    pub allowed: Option<Vec<String>>,
    /// Ignore these providers
    pub ignored: Option<Vec<String>>,
    /// Preferred order
    pub order: Option<Vec<String>>,
    /// Sort criterion
    pub sort: Option<String>,
    /// Require parameters
    pub require_parameters: Option<bool>,
    /// Data collection preference
    pub data_collection: Option<String>,
}

/// Recursively expand ${VAR_NAME} references in config values.
///
/// Mirrors Python `_expand_env_vars` in `hermes_cli/config.py:2487`.
/// Only string values are processed; unresolved references are kept verbatim.
fn expand_env_vars(value: Value) -> Value {
    match value {
        Value::String(s) => {
            static ENV_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let re = ENV_RE.get_or_init(|| regex::Regex::new(r"\$\{([^}]+)\}").unwrap());

            let expanded = re.replace_all(&s, |caps: &regex::Captures| {
                std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
            });
            Value::String(expanded.into_owned())
        }
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, expand_env_vars(v))).collect())
        }
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(expand_env_vars).collect())
        }
        other => other,
    }
}

/// Migrate config through all version steps to LATEST_CONFIG_VERSION.
///
/// Mirrors Python `migrate_config` in `hermes_cli/config.py:2037`.
/// Each step handles the delta between consecutive versions.
fn migrate_config(config: &mut Value) {
    let version = config.get("_config_version")
        .and_then(Value::as_u64)
        .unwrap_or(1) as u32;

    if version >= LATEST_CONFIG_VERSION {
        return;
    }

    for v in version..LATEST_CONFIG_VERSION {
        match v {
            1 => migrate_v1_to_v2(config),
            2 => migrate_v2_to_v3(config),
            3 => migrate_v3_to_v4(config),  // tool progress: .env -> config.yaml
            4 => migrate_v4_to_v5(config),  // add timezone
            5 => migrate_v5_to_v6(config),  // add browser config section
            6 => migrate_v6_to_v7(config),  // add security config
            7 => migrate_v7_to_v8(config),  // add auxiliary model config
            8 => migrate_v8_to_v9(config),  // clear ANTHROPIC_TOKEN from .env
            9 => migrate_v9_to_v10(config), // add Tavily API key
            10 => migrate_v10_to_v11(config), // add terminal.modal_mode
            11 => migrate_v11_to_v12(config), // custom_providers list -> providers dict
            12 => migrate_v12_to_v13(config), // clear dead LLM_MODEL env vars
            13 => migrate_v13_to_v14(config), // migrate legacy stt.model
            14 => migrate_v14_to_v15(config), // display.interim_assistant_messages (Python aligned)
            15 => migrate_v15_to_v16(config), // tool_progress_overrides → display.platforms (Python aligned)
            16 => migrate_v16_to_v17(config), // compression.summary_* → auxiliary.compression (Python aligned)
            17 => migrate_v17_to_v18(config), // add credential_pool_strategies (aligned with Python v18)
            _ => {}
        }
    }

    config["_config_version"] = Value::Number(serde_json::Number::from(LATEST_CONFIG_VERSION));
}

// Migration step implementations (minimal — mostly add missing sections)

fn migrate_v1_to_v2(config: &mut Value) {
    // Add compression section if missing
    if config.get("compression").is_none() {
        config["compression"] = serde_json::json!({"enabled": true, "threshold": 0.50, "target_ratio": 0.20});
    }
}

fn migrate_v2_to_v3(config: &mut Value) {
    // Add terminal section defaults
    if config.get("terminal").is_none() {
        config["terminal"] = serde_json::json!({"backend": "local", "max_output_size": 100000, "lifetime_seconds": 3600});
    }
}

fn migrate_v3_to_v4(_config: &mut Value) {
    // Tool progress migration: read from .env -> config.yaml
    // No-op for Rust; env vars are expanded at load time
}

fn migrate_v4_to_v5(_config: &mut Value) {
    // Add timezone field
    // No-op: timezone is optional, defaults to system
}

fn migrate_v5_to_v6(config: &mut Value) {
    // Add browser config section
    if config.get("browser").is_none() {
        config["browser"] = serde_json::json!({"inactivity_timeout": 120, "command_timeout": 30});
    }
}

fn migrate_v6_to_v7(config: &mut Value) {
    // Add security config
    if config.get("security").is_none() {
        config["security"] = serde_json::json!({"osv_check": false});
    }
}

fn migrate_v7_to_v8(config: &mut Value) {
    // Add auxiliary model config
    if config.get("auxiliary_model").is_none() {
        config["auxiliary_model"] = serde_json::json!({});
    }
}

fn migrate_v8_to_v9(_config: &mut Value) {
    // Clear ANTHROPIC_TOKEN from .env
    // No-op for Rust; env vars are read fresh each load
}

fn migrate_v9_to_v10(_config: &mut Value) {
    // TAVILY_API_KEY added to OPTIONAL_ENV_VARS
    // No-op: env var expansion handles it
}

fn migrate_v10_to_v11(config: &mut Value) {
    // Add terminal.modal_mode
    if let Some(terminal) = config.get_mut("terminal") {
        if terminal.get("modal_mode").is_none() {
            terminal["modal_mode"] = Value::Null;
        }
    }
}

fn migrate_v11_to_v12(config: &mut Value) {
    // Migrate custom_providers (list) -> providers (dict)
    if let Some(providers_list) = config.get("custom_providers").cloned() {
        if let Value::Array(entries) = providers_list {
            let mut providers_map = serde_json::Map::new();
            for (i, entry) in entries.into_iter().enumerate() {
                let name = entry.get("name")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| format!("custom_{}", i));
                providers_map.insert(name, entry);
            }
            config["providers"] = Value::Object(providers_map);
        }
        config.as_object_mut().map(|m| m.remove("custom_providers"));
    }
}

fn migrate_v12_to_v13(_config: &mut Value) {
    // Clear dead LLM_MODEL / OPENAI_MODEL env vars
    // No-op for Rust
}

fn migrate_v13_to_v14(_config: &mut Value) {
    // Migrate legacy flat stt.model to provider section
    // No-op: STT config is platform-specific
}

fn migrate_v14_to_v15(config: &mut Value) {
    // Python v14→v15: add display.interim_assistant_messages=true
    // Mirrors Python config.py:2240-2252
    if let Some(display) = config.get_mut("display") {
        if display.get("interim_assistant_messages").is_none() {
            display["interim_assistant_messages"] = Value::Bool(true);
        }
    } else {
        config["display"] = serde_json::json!({"interim_assistant_messages": true});
    }
}

fn migrate_v15_to_v16(config: &mut Value) {
    // Python v15→v16: migrate tool_progress_overrides → display.platforms
    // Mirrors Python config.py:2254-2276
    if let Some(display) = config.get_mut("display") {
        if let Some(overrides) = display.get("tool_progress_overrides").cloned() {
            if overrides.is_object() {
                let mut platforms = display.get("platforms")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or(Value::Object(serde_json::Map::new()));
                if let (Some(platforms_obj), Some(overrides_obj)) = (platforms.as_object_mut(), overrides.as_object()) {
                    for (plat, mode) in overrides_obj {
                        if !platforms_obj.contains_key(plat) {
                            let mut plat_obj = serde_json::Map::new();
                            plat_obj.insert("tool_progress".to_string(), mode.clone());
                            platforms_obj.insert(plat.clone(), Value::Object(plat_obj));
                        }
                    }
                }
                display["platforms"] = platforms;
            }
            display.as_object_mut().map(|m| m.remove("tool_progress_overrides"));
        }
    }
}

fn migrate_v16_to_v17(config: &mut Value) {
    // Python v16→v17: migrate compression.summary_* → auxiliary.compression
    // Mirrors Python config.py:2278-2313
    if let Some(comp) = config.get_mut("compression") {
        if let Some(comp_obj) = comp.as_object_mut() {
            let s_model = comp_obj.remove("summary_model");
            let s_provider = comp_obj.remove("summary_provider");
            let s_base_url = comp_obj.remove("summary_base_url");

            let should_set_aux = matches!(&s_model, Some(Value::String(s)) if !s.is_empty())
                || matches!(&s_provider, Some(Value::String(s)) if !s.is_empty())
                || matches!(&s_base_url, Some(Value::String(s)) if !s.is_empty());

            if should_set_aux {
                let mut auxiliary = config.get("auxiliary")
                    .cloned()
                    .unwrap_or(Value::Object(serde_json::Map::new()));
                if let Some(aux_obj) = auxiliary.as_object_mut() {
                    if !aux_obj.contains_key("compression") {
                        let mut comp_sub = serde_json::Map::new();
                        if let Some(Some(model)) = s_model.map(|v| v.as_str().map(String::from)) {
                            if !model.is_empty() {
                                comp_sub.insert("model".to_string(), Value::String(model));
                            }
                        }
                        if let Some(Some(provider)) = s_provider.map(|v| v.as_str().map(String::from)) {
                            if !provider.is_empty() {
                                comp_sub.insert("provider".to_string(), Value::String(provider));
                            }
                        }
                        if let Some(Some(base_url)) = s_base_url.map(|v| v.as_str().map(String::from)) {
                            if !base_url.is_empty() {
                                comp_sub.insert("base_url".to_string(), Value::String(base_url));
                            }
                        }
                        if !comp_sub.is_empty() {
                            aux_obj.insert("compression".to_string(), Value::Object(comp_sub));
                        }
                    }
                }
                if let Some(aux) = config.get_mut("auxiliary") {
                    if let Some(new_aux) = auxiliary.as_object() {
                        for (k, v) in new_aux {
                            if let Some(obj) = aux.as_object_mut() {
                                obj.insert(k.clone(), v.clone());
                            }
                        }
                    }
                } else {
                    config["auxiliary"] = auxiliary;
                }
            }
        }
    }
}

fn migrate_v17_to_v18(config: &mut Value) {
    // Rust-specific: add credential_pool_strategies section
    if config.get("credential_pool_strategies").is_none() {
        config["credential_pool_strategies"] = Value::Object(serde_json::Map::new());
    }
}

/// Ensure Rust-specific config fields have their default values.
///
/// These fields are not part of the Python v18 schema, so we unconditionally
/// initialise them after migration so that Rust can rely on their presence.
fn ensure_rust_defaults(config: &mut Value) {
    if config.get("credential_pool_strategies").is_none() {
        config["credential_pool_strategies"] = Value::Object(serde_json::Map::new());
    }
    if config.get("provider").is_none() {
        config["provider"] = Value::Null;
    }
    if config.get("fallback_providers").is_none() {
        config["fallback_providers"] = Value::Array(vec![]);
    }
    if let Some(memory) = config.get_mut("memory") {
        if memory.get("backend").is_none() {
            memory["backend"] = Value::Null;
        }
    }
    if config.get("disabled_tools").is_none() {
        config["disabled_tools"] = Value::Array(vec![]);
    }
}

impl HermesConfig {
    /// Load configuration from the default config file.
    ///
    /// Reads from `~/.hermes/config.yaml` or `./cli-config.yaml` (local override).
    /// Applies config version migration, then expands ${VAR} references.
    /// Falls back to defaults if the file doesn't exist.
    pub fn load() -> Result<Self> {
        let hermes_home = get_hermes_home();
        let config_path = hermes_home.join("config.yaml");

        // Check for local override
        let local_path = std::env::current_dir()?.join("cli-config.yaml");

        let path = if local_path.exists() {
            &local_path
        } else if config_path.exists() {
            &config_path
        } else {
            return Ok(Self::default());
        };

        let content = std::fs::read_to_string(path)
            .map_err(|e| HermesError::with_source(
                ErrorCategory::ConfigError,
                format!("Failed to read config: {}", path.display()),
                e.into(),
            ))?;

        // Parse YAML as JSON Value for manipulation
        let yaml_value: serde_yaml::Value = serde_yaml::from_str(&content)
            .map_err(|e| HermesError::with_source(
                ErrorCategory::ConfigError,
                format!("Failed to parse config: {}", path.display()),
                e.into(),
            ))?;
        let mut json: Value = serde_json::to_value(yaml_value)
            .map_err(|e| HermesError::with_source(
                ErrorCategory::ConfigError,
                "Failed to convert config to JSON".to_string(),
                e.into(),
            ))?;

        // Step 1: Migrate config version
        migrate_config(&mut json);

        // Step 1b: Ensure Rust-specific fields have defaults (not part of Python v18)
        ensure_rust_defaults(&mut json);

        // Step 2: Expand ${VAR} references from environment
        json = expand_env_vars(json);

        // Step 3: Deserialize into typed struct
        let config: HermesConfig = serde_json::from_value(json)
            .map_err(|e| HermesError::with_source(
                ErrorCategory::ConfigError,
                "Failed to deserialize config".to_string(),
                e.into(),
            ))?;

        Ok(config)
    }

    /// Save configuration to the default config file.
    pub fn save(&self) -> Result<()> {
        let hermes_home = get_hermes_home();
        std::fs::create_dir_all(&hermes_home)?;
        let config_path = hermes_home.join("config.yaml");

        // Ensure config version is set on save
        let config = HermesConfig {
            config_version: Some(LATEST_CONFIG_VERSION),
            ..self.clone()
        };

        let content = serde_yaml::to_string(&config)
            .map_err(|e| HermesError::with_source(
                ErrorCategory::ConfigError,
                "Failed to serialize config",
                e.into(),
            ))?;

        std::fs::write(&config_path, content)
            .map_err(|e| HermesError::with_source(
                ErrorCategory::ConfigError,
                format!("Failed to write config: {}", config_path.display()),
                e.into(),
            ))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = HermesConfig::default();
        assert_eq!(config.model.name, Some("anthropic/claude-opus-4-6".to_string()));
        assert_eq!(config.terminal.backend, "local");
        assert_eq!(config.approvals.mode, "smart");
    }

    #[test]
    fn test_config_roundtrip() {
        let config = HermesConfig {
            model: ModelConfig {
                name: Some("openai/gpt-4o".to_string()),
                provider: Some("openai".to_string()),
                ..Default::default()
            },
            terminal: TerminalConfig {
                backend: "docker".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let loaded: HermesConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded.model.name, Some("openai/gpt-4o".to_string()));
        assert_eq!(loaded.terminal.backend, "docker");
    }

    #[test]
    fn test_expand_env_vars_string() {
        std::env::set_var("TEST_API_KEY", "test-key-123");
        let input = Value::String("${TEST_API_KEY}".to_string());
        let output = expand_env_vars(input);
        assert_eq!(output, Value::String("test-key-123".to_string()));
        std::env::remove_var("TEST_API_KEY");
    }

    #[test]
    fn test_expand_env_vars_unresolved_kept() {
        let input = Value::String("${UNLIKELY_VAR_XYZ}".to_string());
        let output = expand_env_vars(input);
        assert_eq!(output, Value::String("${UNLIKELY_VAR_XYZ}".to_string()));
    }

    #[test]
    fn test_expand_env_vars_nested() {
        std::env::set_var("TEST_HOST", "api.example.com");
        std::env::set_var("TEST_PORT", "8080");
        let input = serde_json::json!({
            "host": "${TEST_HOST}",
            "port": "${TEST_PORT}",
            "nested": {
                "url": "https://${TEST_HOST}:${TEST_PORT}"
            }
        });
        let output = expand_env_vars(input);
        assert_eq!(output["host"], Value::String("api.example.com".to_string()));
        assert_eq!(output["port"], Value::String("8080".to_string()));
        assert_eq!(output["nested"]["url"], Value::String("https://api.example.com:8080".to_string()));
        std::env::remove_var("TEST_HOST");
        std::env::remove_var("TEST_PORT");
    }

    #[test]
    fn test_migrate_old_config_to_latest() {
        let mut config = serde_json::json!({
            "_config_version": 1,
            "model": {"name": "openai/gpt-4"},
            "terminal": {"backend": "local"}
        });
        migrate_config(&mut config);
        ensure_rust_defaults(&mut config);
        assert_eq!(config["_config_version"], Value::Number(serde_json::Number::from(18)));
        // Check added sections
        assert!(config.get("compression").is_some());
        assert!(config.get("browser").is_some());
        assert!(config.get("security").is_some());
        assert!(config.get("credential_pool_strategies").is_some());
        assert!(config.get("fallback_providers").is_some());
    }

    #[test]
    fn test_migrate_v11_custom_providers() {
        let mut config = serde_json::json!({
            "_config_version": 11,
            "custom_providers": [
                {"name": "my-provider", "base_url": "https://custom.api.com"}
            ]
        });
        migrate_config(&mut config);
        assert!(config.get("custom_providers").is_none());
        assert!(config.get("providers").is_some());
        assert!(config["providers"].get("my-provider").is_some());
    }

    #[test]
    fn test_migrate_recent_config_no_changes() {
        let mut config = serde_json::json!({
            "_config_version": 18,
            "model": {"name": "anthropic/claude-opus-4-6"}
        });
        migrate_config(&mut config);
        ensure_rust_defaults(&mut config);
        assert_eq!(config["_config_version"], Value::Number(serde_json::Number::from(18)));
        assert_eq!(config["model"]["name"], Value::String("anthropic/claude-opus-4-6".to_string()));
    }
}

/// Coerce a string value to a boolean.
///
/// Recognizes (case-insensitive): `"true"`, `"1"`, `"yes"`, `"on"`.
/// Everything else returns `false`.
pub fn coerce_bool(value: &str) -> bool {
    matches!(value.to_lowercase().trim(), "true" | "1" | "yes" | "on")
}
