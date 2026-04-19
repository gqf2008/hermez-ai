//! Credential pool management.
//!
//! Multiple API keys per provider with rotation on failure, OAuth refresh,
//! exhaustion cooldowns, selection strategies, and persistence.
//!
//! Mirrors the Python credential pool system in `agent/credential_pool.py`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::sync::Mutex;
use parking_lot::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Auth type for a credential entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    OAuth,
    ApiKey,
}

/// Source of a credential entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialSource {
    Manual,
    Env,
    ClaudeCode,
    DeviceCode,
    HermesPkce,
    Custom(String),
}

impl CredentialSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Manual => "manual",
            Self::Env => "env",
            Self::ClaudeCode => "claude_code",
            Self::DeviceCode => "device_code",
            Self::HermesPkce => "hermes_pkce",
            Self::Custom(s) => s,
        }
    }
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Status of a credential entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Ok,
    Exhausted,
}

/// Selection strategy for the credential pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolStrategy {
    /// Use the first available credential (highest priority).
    #[default]
    FillFirst,
    /// Rotate through credentials in order.
    RoundRobin,
    /// Pick a random available credential.
    Random,
    /// Use the least-used credential.
    LeastUsed,
}

/// A single credential entry with full lifecycle state.
///
/// Mirrors Python `PooledCredential` dataclass (credential_pool.py:91).
///
/// **Debug output redacts sensitive fields** (access_token, refresh_token, agent_key)
/// to prevent accidental logging of secrets.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    /// Unique short ID (6 hex chars).
    pub id: String,
    /// Human-readable label (e.g., "primary", "backup").
    pub label: String,
    /// Auth type: oauth or api_key.
    pub auth_type: AuthType,
    /// Priority (lower = higher priority).
    pub priority: u32,
    /// Source of this credential.
    pub source: CredentialSource,
    /// The access token / API key used for requests.
    pub access_token: String,
    /// OAuth refresh token (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Token expiry as ISO-8601 string or epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Token expiry in milliseconds (Anthropic-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Last successful refresh timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
    /// Inference base URL (for Nous/custom providers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_base_url: Option<String>,
    /// Base URL for the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Agent key (Nous-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_key: Option<String>,
    /// Agent key expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_key_expires_at: Option<String>,
    /// Number of requests made with this credential.
    pub request_count: u64,
    /// Last known status (ok / exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<CredentialStatus>,
    /// Timestamp of last status change (epoch seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status_at: Option<f64>,
    /// Last HTTP error code that caused exhaustion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<u16>,
    /// Last error reason string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_reason: Option<String>,
    /// Last error message string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_message: Option<String>,
    /// Provider-supplied reset timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_reset_at: Option<f64>,
    /// Extension fields for provider-specific data.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("auth_type", &self.auth_type)
            .field("priority", &self.priority)
            .field("source", &self.source)
            .field("access_token", &"***REDACTED***")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***REDACTED***"))
            .field("expires_at", &self.expires_at)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("last_refresh", &self.last_refresh)
            .field("inference_base_url", &self.inference_base_url)
            .field("base_url", &self.base_url)
            .field("agent_key", &self.agent_key.as_ref().map(|_| "***REDACTED***"))
            .field("agent_key_expires_at", &self.agent_key_expires_at)
            .field("request_count", &self.request_count)
            .field("last_status", &self.last_status)
            .field("last_status_at", &self.last_status_at)
            .field("last_error_code", &self.last_error_code)
            .field("last_error_reason", &self.last_error_reason)
            .field("last_error_message", &self.last_error_message)
            .field("last_error_reset_at", &self.last_error_reset_at)
            .field("extra", &self.extra)
            .finish()
    }
}

impl Credential {
    /// Create a new credential with auto-generated ID.
    pub fn new(api_key: String) -> Self {
        Self {
            id: Self::generate_id(),
            label: "default".to_string(),
            auth_type: AuthType::ApiKey,
            priority: 0,
            source: CredentialSource::Manual,
            access_token: api_key,
            refresh_token: None,
            expires_at: None,
            expires_at_ms: None,
            last_refresh: None,
            inference_base_url: None,
            base_url: None,
            agent_key: None,
            agent_key_expires_at: None,
            request_count: 0,
            last_status: None,
            last_status_at: None,
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at: None,
            extra: HashMap::new(),
        }
    }

    /// Generate a short 6-char hex ID.
    fn generate_id() -> String {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64);
        format!("{:06x}", hasher.finish() % 0x1000000)
    }

    /// Get the runtime API key for this credential.
    /// For Nous providers, prefers agent_key over access_token.
    pub fn runtime_api_key(&self) -> &str {
        if self.agent_key.as_ref().is_some_and(|k| !k.is_empty()) {
            return self.agent_key.as_ref().unwrap();
        }
        &self.access_token
    }

    /// Get the runtime base URL for this credential.
    /// For Nous providers, prefers inference_base_url.
    pub fn runtime_base_url(&self) -> Option<&str> {
        self.inference_base_url
            .as_deref()
            .or(self.base_url.as_deref())
    }

    /// Check if this credential is currently exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.last_status == Some(CredentialStatus::Exhausted)
    }

    /// Calculate when this credential's exhaustion cooldown ends.
    /// Returns epoch seconds, or None if not exhausted or cooldown expired.
    pub fn exhausted_until(&self) -> Option<f64> {
        if self.last_status != Some(CredentialStatus::Exhausted) {
            return None;
        }
        // Provider-supplied reset_at
        if let Some(reset_at) = self.last_error_reset_at {
            if reset_at > 0.0 {
                return Some(reset_at);
            }
        }
        // Default cooldown
        if let Some(status_at) = self.last_status_at {
            let ttl = exhausted_ttl_seconds(self.last_error_code);
            return Some(status_at + ttl);
        }
        None
    }

    /// Check if cooldown has elapsed.
    pub fn cooldown_expired(&self) -> bool {
        match self.exhausted_until() {
            Some(until) => now_epoch() >= until,
            None => true, // Not exhausted = available
        }
    }

    /// Increment request counter.
    pub fn record_request(&mut self) {
        self.request_count += 1;
    }
}

/// Cooldown TTL based on the HTTP status that caused exhaustion.
fn exhausted_ttl_seconds(error_code: Option<u16>) -> f64 {
    match error_code {
        Some(429) => 3600.0, // 1 hour for rate limit
        _ => 3600.0,         // 1 hour default
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Pool of credentials for a single provider.
///
/// Supports multiple selection strategies, exhaustion tracking with cooldown,
/// lease management, and persistence.
///
/// Mirrors Python `CredentialPool` class (credential_pool.py:365).
pub struct CredentialPool {
    pub provider: String,
    entries: RwLock<Vec<Credential>>,
    current_id: Mutex<Option<String>>,
    strategy: PoolStrategy,
    index: AtomicUsize, // For round-robin tracking
    #[allow(dead_code)]
    active_leases: Mutex<HashMap<String, u32>>,
    #[allow(dead_code)]
    max_concurrent: u32,
}

impl Clone for CredentialPool {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            entries: RwLock::new(self.entries.read().clone()),
            current_id: Mutex::new(self.current_id.lock().ok().and_then(|g| (*g).clone())),
            strategy: self.strategy,
            index: AtomicUsize::new(self.index.load(std::sync::atomic::Ordering::SeqCst)),
            active_leases: Mutex::new(HashMap::new()),
            max_concurrent: self.max_concurrent,
        }
    }
}

impl std::fmt::Debug for CredentialPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPool")
            .field("provider", &self.provider)
            .field("entries", &self.entries.read().len())
            .field("strategy", &self.strategy)
            .finish()
    }
}

impl CredentialPool {
    /// Create a new credential pool.
    pub fn new(provider: String, entries: Vec<Credential>) -> Self {
        let strategy = PoolStrategy::default();
        let mut sorted = entries;
        sorted.sort_by_key(|e| e.priority);
        // Re-assign priorities after sort
        for (idx, entry) in sorted.iter_mut().enumerate() {
            entry.priority = idx as u32;
        }
        Self {
            provider,
            entries: RwLock::new(sorted),
            current_id: Mutex::new(None),
            strategy,
            index: AtomicUsize::new(0),
            active_leases: Mutex::new(HashMap::new()),
            max_concurrent: 1,
        }
    }

    /// Create a pool with a specific strategy.
    pub fn with_strategy(provider: String, entries: Vec<Credential>, strategy: PoolStrategy) -> Self {
        let mut sorted = entries;
        sorted.sort_by_key(|e| e.priority);
        for (idx, entry) in sorted.iter_mut().enumerate() {
            entry.priority = idx as u32;
        }
        Self {
            provider,
            entries: RwLock::new(sorted),
            current_id: Mutex::new(None),
            strategy,
            index: AtomicUsize::new(0),
            active_leases: Mutex::new(HashMap::new()),
            max_concurrent: 1,
        }
    }

    /// Check if the pool has any credentials.
    pub fn has_credentials(&self) -> bool {
        !self.entries.read().is_empty()
    }

    /// Check if any credential is available (not in exhaustion cooldown).
    pub fn has_available(&self) -> bool {
        self.entries.read().iter().any(|e| !e.is_exhausted() || e.cooldown_expired())
    }

    /// Number of credentials in the pool.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Get all entries (cloned).
    pub fn entries(&self) -> Vec<Credential> {
        self.entries.read().clone()
    }

    /// Get the currently selected credential.
    pub fn current(&self) -> Option<Credential> {
        let guard = self.current_id.lock().ok()?;
        let current_id = guard.as_ref()?;
        self.entries.read().iter().find(|e| e.id == *current_id).cloned()
    }

    /// Select the next credential using the configured strategy.
    pub fn select(&self) -> Option<Credential> {
        let available: Vec<(usize, Credential)> = self.entries.read().iter()
            .enumerate()
            .filter(|(_, e)| !e.is_exhausted() || e.cooldown_expired())
            .map(|(i, e)| (i, e.clone()))
            .collect();
        if available.is_empty() {
            *self.current_id.lock().ok()? = None;
            return None;
        }

        let selected = match self.strategy {
            PoolStrategy::Random => {
                let idx = fastrand::usize(..available.len());
                Some(available[idx].1.clone())
            }
            PoolStrategy::LeastUsed if available.len() > 1 => {
                available.iter().min_by_key(|(_, e)| e.request_count).map(|(_, c)| c.clone())
            }
            PoolStrategy::RoundRobin if available.len() > 1 => {
                let idx = self.index.fetch_add(1, Ordering::SeqCst) % available.len();
                Some(available[idx].1.clone())
            }
            _ => Some(available[0].1.clone()), // fill_first default
        };

        if let Some(ref entry) = selected {
            *self.current_id.lock().ok()? = Some(entry.id.clone());
        }
        selected
    }

    /// Select the first credential without advancing.
    pub fn first(&self) -> Option<Credential> {
        self.entries.read().first().cloned()
    }

    /// Reset the round-robin index to 0.
    pub fn reset(&self) {
        self.index.store(0, Ordering::SeqCst);
    }

    /// Mark current credential as exhausted and select the next available one.
    ///
    /// Mirrors Python `mark_exhausted_and_rotate` (credential_pool.py:867).
    pub fn mark_exhausted_and_rotate(
        &self,
        status_code: Option<u16>,
        error_context: Option<&serde_json::Value>,
    ) -> Option<Credential> {
        // Find current entry and mark exhausted
        {
            let guard = self.current_id.lock().ok()?;
            let current_id = guard.as_ref()?;
            let mut entries = self.entries.write();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == *current_id) {
                let normalized = normalize_error_context(error_context);
                entry.last_status = Some(CredentialStatus::Exhausted);
                entry.last_status_at = Some(now_epoch());
                entry.last_error_code = status_code;
                entry.last_error_reason = normalized.get("reason").and_then(|v| v.as_str()).map(String::from);
                entry.last_error_message = normalized.get("message").and_then(|v| v.as_str()).map(String::from);
                entry.last_error_reset_at = normalized.get("reset_at").and_then(|v| v.as_f64());
            }
        }

        // Clear current and select next
        *self.current_id.lock().ok()? = None;
        self.select()
    }

    /// Try to refresh the current credential (OAuth).
    /// Returns true if refresh was attempted and succeeded.
    ///
    /// Mirrors Python `_try_refresh_current_unlocked` (credential_pool.py:934).
    /// For OAuth credentials, calls `refresh_anthropic_oauth_pure` and updates
    /// the credential's access_token, refresh_token, and expires_at_ms in-place.
    pub async fn try_refresh_current(&self) -> bool {
        let current = self.current_by_index();
        let Some(cred) = current else {
            return false;
        };

        // Only OAuth credentials can be refreshed
        if cred.auth_type != AuthType::OAuth {
            return false;
        }

        let Some(ref refresh_token) = cred.refresh_token else {
            return false;
        };
        let refresh_token = refresh_token.clone();
        let cred_id = cred.id.clone();

        // Call the OAuth refresh function
        match crate::anthropic::refresh_anthropic_oauth_pure(&refresh_token, false).await {
            Ok(refreshed) => {
                // Update the credential in-place
                let mut entries = self.entries.write();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == cred_id) {
                    entry.access_token = refreshed.access_token;
                    entry.refresh_token = Some(refreshed.refresh_token);
                    entry.expires_at_ms = Some(refreshed.expires_at_ms);
                    entry.last_refresh = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs().to_string())
                            .unwrap_or_default(),
                    );
                    tracing::info!(
                        "Credential pool: refreshed OAuth token for {} ({})",
                        entry.label, entry.id
                    );
                }
                true
            }
            Err(e) => {
                tracing::warn!("Credential pool: OAuth refresh failed: {}", e);
                false
            }
        }
    }

    /// Get the credential at the round-robin index without advancing.
    pub fn current_by_index(&self) -> Option<Credential> {
        let entries = self.entries.read();
        if entries.is_empty() {
            return None;
        }
        let idx = self.index.load(Ordering::SeqCst) % entries.len();
        entries.get(idx).cloned()
    }

    /// Clear expired exhaustion cooldowns and persist.
    pub fn clear_expired_cooldowns(&self) {
        let mut cleared = false;
        for entry in self.entries.write().iter_mut() {
            if entry.is_exhausted() && entry.cooldown_expired() {
                entry.last_status = Some(CredentialStatus::Ok);
                entry.last_status_at = None;
                entry.last_error_code = None;
                entry.last_error_reason = None;
                entry.last_error_message = None;
                entry.last_error_reset_at = None;
                cleared = true;
            }
        }
        if cleared {
            // Could persist here if needed
        }
    }

    /// Reset all error statuses on all entries.
    /// Returns the number of entries that were modified.
    pub fn reset_statuses(&self) -> usize {
        let mut count = 0;
        for entry in self.entries.write().iter_mut() {
            if entry.last_status.is_some()
                || entry.last_status_at.is_some()
                || entry.last_error_code.is_some()
            {
                entry.last_status = None;
                entry.last_status_at = None;
                entry.last_error_code = None;
                entry.last_error_reason = None;
                entry.last_error_message = None;
                entry.last_error_reset_at = None;
                count += 1;
            }
        }
        count
    }

    /// Remove an entry by index (1-based).
    pub fn remove_index(&self, index: usize) -> Option<Credential> {
        let mut entries = self.entries.write();
        if index == 0 || index > entries.len() {
            return None;
        }
        let removed = entries.remove(index - 1).clone();
        // Re-assign priorities
        for (idx, entry) in entries.iter_mut().enumerate() {
            entry.priority = idx as u32;
        }
        Some(removed)
    }

    /// Add a new entry to the pool.
    pub fn add_entry(&self, mut entry: Credential) -> Credential {
        let mut entries = self.entries.write();
        let next_priority = entries.iter().map(|e| e.priority).max().unwrap_or(0) + 1;
        entry.priority = next_priority;
        entries.push(entry);
        entries.last().unwrap().clone()
    }

    /// Resolve a credential target by ID, label, or numeric index.
    ///
    /// Mirrors Python `resolve_target` (credential_pool.py:980).
    pub fn resolve_target(&self, target: &str) -> anyhow::Result<(usize, Credential)> {
        let raw = target.trim();
        if raw.is_empty() {
            return Err(anyhow::anyhow!("No credential target provided."));
        }

        let entries = self.entries.read();

        // Match by ID
        for (idx, entry) in entries.iter().enumerate() {
            if entry.id == raw {
                return Ok((idx + 1, entry.clone()));
            }
        }

        // Match by label (case-insensitive)
        let label_matches: Vec<(usize, Credential)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.label.to_lowercase() == raw.to_lowercase())
            .map(|(i, e)| (i, e.clone()))
            .collect();

        if label_matches.len() == 1 {
            return Ok((label_matches[0].0 + 1, label_matches[0].1.clone()));
        }
        if label_matches.len() > 1 {
            return Err(anyhow::anyhow!("Ambiguous label \"{raw}\". Use ID or index instead."));
        }

        // Match by numeric index
        if let Ok(index) = raw.parse::<usize>() {
            if (1..=entries.len()).contains(&index) {
                return Ok((index, entries[index - 1].clone()));
            }
            return Err(anyhow::anyhow!("No credential #{index}."));
        }

        Err(anyhow::anyhow!("No credential matching \"{raw}\"."))
    }

    /// Get the selection strategy.
    pub fn strategy(&self) -> PoolStrategy {
        self.strategy
    }

    /// Set the selection strategy.
    pub fn set_strategy(&mut self, strategy: PoolStrategy) {
        self.strategy = strategy;
    }

    /// Serialize the pool to JSON for persistence.
    pub fn to_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(&*self.entries.read())
    }

    /// Get the current round-robin index.
    pub fn current_index(&self) -> usize {
        self.index.load(Ordering::SeqCst)
    }
}

/// Normalize error context from provider responses.
///
/// Mirrors Python `_normalize_error_context` (credential_pool.py:242).
fn normalize_error_context(error_context: Option<&serde_json::Value>) -> HashMap<String, serde_json::Value> {
    let mut normalized = HashMap::new();
    let Some(obj) = error_context.and_then(|v| v.as_object()) else {
        return normalized;
    };

    if let Some(reason) = obj.get("reason").and_then(|v| v.as_str()) {
        let trimmed = reason.trim();
        if !trimmed.is_empty() {
            normalized.insert("reason".into(), trimmed.into());
        }
    }
    if let Some(message) = obj.get("message").and_then(|v| v.as_str()) {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            normalized.insert("message".into(), trimmed.into());
        }
    }

    // Parse reset_at from various field names
    let reset_at = obj.get("reset_at")
        .or(obj.get("resets_at"))
        .or(obj.get("retry_until"));
    if let Some(val) = reset_at {
        if let Some(parsed) = parse_absolute_timestamp(val) {
            normalized.insert("reset_at".into(), parsed.into());
        } else if let Some(msg) = obj.get("message").and_then(|v| v.as_str()) {
            if let Some(delay) = extract_retry_delay_seconds(msg) {
                normalized.insert("reset_at".into(), (now_epoch() + delay).into());
            }
        }
    }

    normalized
}

/// Parse an absolute timestamp from various formats.
///
/// Mirrors Python `_parse_absolute_timestamp` (credential_pool.py:199).
fn parse_absolute_timestamp(value: &serde_json::Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        if n <= 0.0 {
            return None;
        }
        // Distinguish epoch seconds vs milliseconds
        return Some(if n > 1_000_000_000_000.0 { n / 1000.0 } else { n });
    }
    if let Some(s) = value.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Try numeric first
        if let Ok(n) = trimmed.parse::<f64>() {
            return Some(if n > 1_000_000_000_000.0 { n / 1000.0 } else { n });
        }
        // Try ISO-8601
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
            return Some(dt.timestamp() as f64);
        }
    }
    None
}

static RETRY_DELAY_RE_QUOTA: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"quotaResetDelay[:\s""]+(\d+(?:\.\d+)?)(ms|s)"#)
        .expect("static retry delay regex is valid")
});

static RETRY_DELAY_RE_AFTER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"retry\s+(?:after\s+)?(\d+(?:\.\d+)?)\s*(?:sec|secs|seconds|s\b)")
        .expect("static retry delay regex is valid")
});

/// Extract retry delay from error message patterns.
///
/// Mirrors Python `_extract_retry_delay_seconds` (credential_pool.py:229).
fn extract_retry_delay_seconds(message: &str) -> Option<f64> {
    // Pattern: quotaResetDelay: 5000ms or "quotaResetDelay": 5s
    if let Some(caps) = RETRY_DELAY_RE_QUOTA.captures(message) {
        let value: f64 = caps[1].parse().ok()?;
        return Some(if &caps[2].to_lowercase() == "ms" { value / 1000.0 } else { value });
    }
    // Pattern: retry after 30 sec
    if let Some(caps) = RETRY_DELAY_RE_AFTER.captures(message) {
        return caps[1].parse().ok();
    }
    None
}

/// Load credentials from environment variables for a given provider.
///
/// Supports multiple comma-separated API keys in a single env var,
/// creating one credential entry per key.
pub fn load_from_env(provider: &str) -> Option<CredentialPool> {
    let env_vars = match provider {
        "openrouter" => vec!["OPENROUTER_API_KEY"],
        "nous" => vec!["NOUS_API_KEY"],
        "openai" | "openai-codex" => vec!["OPENAI_API_KEY"],
        "anthropic" => vec!["ANTHROPIC_API_KEY", "ANTHROPIC_TOKEN"],
        "gemini" => vec!["GEMINI_API_KEY"],
        _ => return None,
    };

    let mut credentials = Vec::new();
    for env_var in &env_vars {
        if let Ok(value) = std::env::var(env_var) {
            // Support comma-separated keys
            for key in value.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                credentials.push(Credential {
                    id: Credential::generate_id(),
                    label: format!("env:{env_var}"),
                    auth_type: AuthType::ApiKey,
                    priority: credentials.len() as u32,
                    source: CredentialSource::Env,
                    access_token: key.to_string(),
                    refresh_token: None,
                    expires_at: None,
                    expires_at_ms: None,
                    last_refresh: None,
                    inference_base_url: None,
                    base_url: None,
                    agent_key: None,
                    agent_key_expires_at: None,
                    request_count: 0,
                    last_status: None,
                    last_status_at: None,
                    last_error_code: None,
                    last_error_reason: None,
                    last_error_message: None,
                    last_error_reset_at: None,
                    extra: HashMap::new(),
                });
            }
        }
    }

    if credentials.is_empty() {
        return None;
    }

    Some(CredentialPool::new(provider.to_string(), credentials))
}

/// Build a credential pool from a list of JSON entries.
///
/// Mirrors Python `PooledCredential.from_dict` (credential_pool.py:127).
pub fn from_entries(provider: &str, entries: Vec<serde_json::Value>) -> Option<CredentialPool> {
    let credentials: Vec<Credential> = entries
        .into_iter()
        .filter_map(|entry| {
            let obj = entry.as_object()?;

            // Must have access_token or api_key
            let access_token = obj
                .get("access_token")
                .or(obj.get("api_key"))?
                .as_str()?
                .to_string();

            let id = obj.get("id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(Credential::generate_id);

            let label = obj.get("label")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| provider.to_string());

            let auth_type = obj.get("auth_type")
                .and_then(|v| v.as_str())
                .map(|s| match s {
                    "oauth" => AuthType::OAuth,
                    _ => AuthType::ApiKey,
                })
                .unwrap_or(AuthType::ApiKey);

            let priority = obj.get("priority")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let source = obj.get("source")
                .and_then(|v| v.as_str())
                .map(|s| match s {
                    "manual" => CredentialSource::Manual,
                    "env" => CredentialSource::Env,
                    "claude_code" => CredentialSource::ClaudeCode,
                    "device_code" => CredentialSource::DeviceCode,
                    "hermes_pkce" => CredentialSource::HermesPkce,
                    other => CredentialSource::Custom(other.to_string()),
                })
                .unwrap_or(CredentialSource::Manual);

            let refresh_token = obj.get("refresh_token").and_then(|v| v.as_str()).map(String::from);
            let expires_at = obj.get("expires_at").and_then(|v| v.as_str()).map(String::from);
            let expires_at_ms = obj.get("expires_at_ms").and_then(|v| v.as_u64());
            let last_refresh = obj.get("last_refresh").and_then(|v| v.as_str()).map(String::from);
            let inference_base_url = obj.get("inference_base_url").and_then(|v| v.as_str()).map(String::from);
            let base_url = obj.get("base_url").and_then(|v| v.as_str()).map(String::from);
            let agent_key = obj.get("agent_key").and_then(|v| v.as_str()).map(String::from);
            let agent_key_expires_at = obj.get("agent_key_expires_at").and_then(|v| v.as_str()).map(String::from);

            let request_count = obj.get("request_count").and_then(|v| v.as_u64()).unwrap_or(0);

            // Parse status fields
            let last_status = obj.get("last_status").and_then(|v| v.as_str()).map(|s| match s {
                "ok" => CredentialStatus::Ok,
                "exhausted" => CredentialStatus::Exhausted,
                _ => CredentialStatus::Ok,
            });
            let last_status_at = obj.get("last_status_at").and_then(|v| v.as_f64());
            let last_error_code = obj.get("last_error_code").and_then(|v| v.as_u64()).map(|v| v as u16);
            let last_error_reason = obj.get("last_error_reason").and_then(|v| v.as_str()).map(String::from);
            let last_error_message = obj.get("last_error_message").and_then(|v| v.as_str()).map(String::from);
            let last_error_reset_at = obj.get("last_error_reset_at").and_then(|v| v.as_f64());

            // Collect extra fields
            let known_keys = [
                "id", "label", "auth_type", "priority", "source", "access_token",
                "refresh_token", "expires_at", "expires_at_ms", "last_refresh",
                "inference_base_url", "base_url", "agent_key", "agent_key_expires_at",
                "request_count", "last_status", "last_status_at", "last_error_code",
                "last_error_reason", "last_error_message", "last_error_reset_at",
                "api_key", // alias, already handled
            ];
            let mut extra = HashMap::new();
            for (k, v) in obj {
                if !known_keys.contains(&k.as_str()) {
                    extra.insert(k.clone(), v.clone());
                }
            }

            Some(Credential {
                id,
                label,
                auth_type,
                priority,
                source,
                access_token,
                refresh_token,
                expires_at,
                expires_at_ms,
                last_refresh,
                inference_base_url,
                base_url,
                agent_key,
                agent_key_expires_at,
                request_count,
                last_status,
                last_status_at,
                last_error_code,
                last_error_reason,
                last_error_message,
                last_error_reset_at,
                extra,
            })
        })
        .collect();

    if credentials.is_empty() {
        return None;
    }

    Some(CredentialPool::new(provider.to_string(), credentials))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_credential(key: &str) -> Credential {
        Credential {
            id: Credential::generate_id(),
            label: key.to_string(),
            auth_type: AuthType::ApiKey,
            priority: 0,
            source: CredentialSource::Manual,
            access_token: key.to_string(),
            ..Default::default()
        }
    }

    impl Default for Credential {
        fn default() -> Self {
            Self {
                id: Credential::generate_id(),
                label: "default".to_string(),
                auth_type: AuthType::ApiKey,
                priority: 0,
                source: CredentialSource::Manual,
                access_token: String::new(),
                refresh_token: None,
                expires_at: None,
                expires_at_ms: None,
                last_refresh: None,
                inference_base_url: None,
                base_url: None,
                agent_key: None,
                agent_key_expires_at: None,
                request_count: 0,
                last_status: None,
                last_status_at: None,
                last_error_code: None,
                last_error_reason: None,
                last_error_message: None,
                last_error_reset_at: None,
                extra: HashMap::new(),
            }
        }
    }

    #[test]
    fn test_pool_round_robin() {
        let pool = CredentialPool::with_strategy("test".to_string(), vec![
            make_credential("key1"),
            make_credential("key2"),
            make_credential("key3"),
        ], PoolStrategy::RoundRobin);

        assert_eq!(pool.select().unwrap().access_token, "key1");
        assert_eq!(pool.select().unwrap().access_token, "key2");
        assert_eq!(pool.select().unwrap().access_token, "key3");
        // Wraps around
        assert_eq!(pool.select().unwrap().access_token, "key1");
    }

    #[test]
    fn test_pool_empty() {
        let pool = CredentialPool::new("test".to_string(), vec![]);
        assert!(pool.select().is_none());
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn test_pool_reset() {
        let pool = CredentialPool::new("test".to_string(), vec![
            make_credential("a"),
            make_credential("b"),
        ]);
        pool.select();
        pool.select();
        pool.reset();
        assert_eq!(pool.select().unwrap().access_token, "a");
    }

    #[test]
    fn test_from_entries() {
        let entries = vec![
            serde_json::json!({ "api_key": "key1", "label": "primary" }),
            serde_json::json!({ "api_key": "key2", "base_url": "https://custom.api.com" }),
        ];
        let pool = from_entries("openai", entries).unwrap();
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.first().unwrap().access_token, "key1");
    }

    #[test]
    fn test_from_entries_empty() {
        let pool = from_entries("openai", vec![]);
        assert!(pool.is_none());
    }

    #[test]
    fn test_mark_exhausted_and_rotate() {
        let pool = CredentialPool::new("test".to_string(), vec![
            make_credential("key1"),
            make_credential("key2"),
            make_credential("key3"),
        ]);

        assert_eq!(pool.select().unwrap().access_token, "key1");
        let next = pool.mark_exhausted_and_rotate(Some(429), None);
        assert_eq!(next.unwrap().access_token, "key2");
        let next = pool.mark_exhausted_and_rotate(Some(402), None);
        assert_eq!(next.unwrap().access_token, "key3");
        // Wraps around to first (key1's cooldown is still active, so skip to available)
        // Since cooldown is 1 hour, key1 is still exhausted
        assert!(pool.has_available()); // key1 is exhausted but cooldown hasn't expired
    }

    #[test]
    fn test_mark_exhausted_single_credential() {
        let pool = CredentialPool::new("test".to_string(), vec![
            make_credential("only-key"),
        ]);
        assert!(pool.mark_exhausted_and_rotate(Some(429), None).is_none());
    }

    #[tokio::test]
    async fn test_try_refresh_current_default() {
        let pool = CredentialPool::new("test".to_string(), vec![
            make_credential("key"),
        ]);
        // Non-OAuth credential → returns false
        assert!(!pool.try_refresh_current().await);
    }

    #[test]
    fn test_exhaustion_cooldown() {
        let mut cred = make_credential("test-key");
        assert!(!cred.is_exhausted());
        assert!(cred.cooldown_expired());

        cred.last_status = Some(CredentialStatus::Exhausted);
        cred.last_status_at = Some(now_epoch());
        cred.last_error_code = Some(429);

        assert!(cred.is_exhausted());
        assert!(!cred.cooldown_expired()); // Just set, cooldown active
    }

    #[test]
    fn test_exhausted_until_with_reset_at() {
        let mut cred = make_credential("test-key");
        cred.last_status = Some(CredentialStatus::Exhausted);
        cred.last_status_at = Some(1000.0);
        cred.last_error_reset_at = Some(2000.0);

        assert_eq!(cred.exhausted_until(), Some(2000.0));
    }

    #[test]
    fn test_exhausted_until_default_ttl() {
        let mut cred = make_credential("test-key");
        let now = now_epoch();
        cred.last_status = Some(CredentialStatus::Exhausted);
        cred.last_status_at = Some(now);

        let until = cred.exhausted_until().unwrap();
        assert!((until - (now + 3600.0)).abs() < 1.0); // Within 1 second
    }

    #[test]
    fn test_runtime_api_key_nous() {
        let mut cred = make_credential("access-token");
        cred.agent_key = Some("agent-key".to_string());
        // For Nous provider, agent_key should be preferred
        assert_eq!(cred.runtime_api_key(), "agent-key");

        cred.agent_key = None;
        assert_eq!(cred.runtime_api_key(), "access-token");
    }

    #[test]
    fn test_runtime_base_url_nous() {
        let mut cred = make_credential("key");
        cred.base_url = Some("https://base.url".to_string());
        cred.inference_base_url = Some("https://inference.url".to_string());
        assert_eq!(cred.runtime_base_url(), Some("https://inference.url"));

        cred.inference_base_url = None;
        assert_eq!(cred.runtime_base_url(), Some("https://base.url"));
    }

    #[test]
    fn test_from_entries_with_full_schema() {
        let entries = vec![serde_json::json!({
            "id": "abc123",
            "label": "my-oauth",
            "auth_type": "oauth",
            "priority": 5,
            "source": "claude_code",
            "access_token": "at-123",
            "refresh_token": "rt-456",
            "expires_at_ms": 1700000000000u64,
            "request_count": 42,
            "last_status": "ok",
            "extra_field": "value"
        })];
        let pool = from_entries("anthropic", entries).unwrap();
        let cred = pool.first().unwrap();
        assert_eq!(cred.id, "abc123");
        assert_eq!(cred.label, "my-oauth");
        assert_eq!(cred.auth_type, AuthType::OAuth);
        assert_eq!(cred.priority, 0); // Pool re-assigns priorities on construction
        assert_eq!(cred.access_token, "at-123");
        assert_eq!(cred.refresh_token.as_deref(), Some("rt-456"));
        assert_eq!(cred.expires_at_ms, Some(1700000000000));
        assert_eq!(cred.request_count, 42);
        assert_eq!(cred.last_status, Some(CredentialStatus::Ok));
    }

    #[test]
    fn test_normalize_error_context() {
        let ctx = serde_json::json!({
            "reason": "rate_limited",
            "message": "Too many requests",
            "reset_at": 1700000000.0
        });
        let normalized = normalize_error_context(Some(&ctx));
        assert_eq!(normalized["reason"], "rate_limited");
        assert_eq!(normalized["message"], "Too many requests");
        assert_eq!(normalized["reset_at"], 1700000000.0);
    }

    #[test]
    fn test_normalize_error_context_empty() {
        let normalized = normalize_error_context(None);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_extract_retry_delay_seconds() {
        assert_eq!(extract_retry_delay_seconds("quotaResetDelay: 5000ms"), Some(5.0));
        assert_eq!(extract_retry_delay_seconds("quotaResetDelay: 30s"), Some(30.0));
        assert_eq!(extract_retry_delay_seconds("please retry after 10 seconds"), Some(10.0));
        assert_eq!(extract_retry_delay_seconds("some other error"), None);
    }

    #[test]
    fn test_pool_fill_first() {
        let pool = CredentialPool::with_strategy(
            "test".to_string(),
            vec![make_credential("key1"), make_credential("key2")],
            PoolStrategy::FillFirst,
        );
        // Fill first always returns the first available
        assert_eq!(pool.select().unwrap().access_token, "key1");
        assert_eq!(pool.select().unwrap().access_token, "key1");
    }

    #[test]
    fn test_resolve_target() {
        let pool = CredentialPool::new("test".to_string(), vec![
            Credential { id: "abc123".to_string(), label: "primary".to_string(), ..make_credential("key1") },
            Credential { id: "def456".to_string(), label: "backup".to_string(), ..make_credential("key2") },
        ]);

        // By ID
        let (idx, cred) = pool.resolve_target("abc123").unwrap();
        assert_eq!(idx, 1);
        assert_eq!(cred.access_token, "key1");

        // By label
        let (idx, cred) = pool.resolve_target("backup").unwrap();
        assert_eq!(idx, 2);
        assert_eq!(cred.access_token, "key2");

        // By index
        let (idx, cred) = pool.resolve_target("1").unwrap();
        assert_eq!(idx, 1);
        assert_eq!(cred.access_token, "key1");

        // Not found
        assert!(pool.resolve_target("nonexistent").is_err());
    }
}
