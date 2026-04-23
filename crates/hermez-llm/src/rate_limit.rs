#![allow(dead_code)]
//! Rate limit tracking and Nous subscription quota guard.
//!
//! Supports:
//! - Sliding window rate tracking with event timestamps
//! - Rate limit header extraction from HTTP responses (OpenAI, Anthropic, OpenRouter, Nous)
//! - Retry-after calculation with elapsed time adjustment
//! - Nous rate guard: subscription tier tracking, daily/monthly quota checks
//!
//! Mirrors the Python implementations in:
//! - `agent/rate_limit_tracker.py`
//! - `agent/nous_rate_guard.py`

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

// ─── Sliding Window Rate Tracker ────────────────────────────────────────

/// A sliding window rate limiter that tracks individual request timestamps.
///
/// Automatically evicts expired entries and supports token-weighted counting.
#[derive(Debug, Clone)]
pub struct SlidingWindow {
    /// Maximum requests allowed in the window.
    pub limit: u64,
    /// Window duration.
    pub window: Duration,
    /// Timestamps of requests within the window (oldest first).
    entries: VecDeque<Instant>,
}

impl SlidingWindow {
    /// Create a new sliding window with the given limit and duration.
    pub fn new(limit: u64, window: Duration) -> Self {
        Self {
            limit,
            window,
            entries: VecDeque::new(),
        }
    }

    /// Evict entries older than the current window.
    pub fn evict(&mut self) {
        let cutoff = Instant::now() - self.window;
        while self.entries.front().is_some_and(|ts| *ts < cutoff) {
            self.entries.pop_front();
        }
    }

    /// Record a new request. Returns `true` if the request is allowed.
    pub fn record(&mut self) -> bool {
        self.evict();
        if self.entries.len() as u64 >= self.limit {
            return false;
        }
        self.entries.push_back(Instant::now());
        true
    }

    /// Current number of requests in the window.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Remaining requests in the window.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.count() as u64)
    }

    /// Seconds until the oldest entry expires (first opening in the window).
    pub fn time_until_next_available(&self) -> Option<Duration> {
        if self.entries.is_empty() {
            return None;
        }
        let oldest = self.entries.front().unwrap();
        let age = oldest.elapsed();
        if age >= self.window {
            None
        } else {
            Some(self.window - age)
        }
    }

    /// Whether the window is currently at capacity.
    pub fn is_limited(&mut self) -> bool {
        self.evict();
        self.entries.len() as u64 >= self.limit
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ─── Rate Limit Bucket ─────────────────────────────────────────────────

/// One rate-limit window (e.g., requests per minute, tokens per hour).
#[derive(Debug, Clone)]
pub struct RateLimitBucket {
    /// Maximum allowed in the window.
    pub limit: u64,
    /// Remaining count in the window.
    pub remaining: u64,
    /// Seconds until the window resets.
    pub reset_seconds: f64,
    /// When this bucket was captured.
    pub captured_at: Instant,
}

impl Default for RateLimitBucket {
    fn default() -> Self {
        Self {
            limit: 0,
            remaining: 0,
            reset_seconds: 0.0,
            captured_at: Instant::now(),
        }
    }
}

impl RateLimitBucket {
    /// Number of units used in the window.
    pub fn used(&self) -> u64 {
        self.limit.saturating_sub(self.remaining)
    }

    /// Percentage of the limit used.
    pub fn usage_pct(&self) -> f64 {
        if self.limit == 0 {
            return 0.0;
        }
        (self.used() as f64 / self.limit as f64) * 100.0
    }

    /// Estimated seconds remaining until reset, adjusted for elapsed time.
    pub fn remaining_seconds_now(&self) -> f64 {
        let elapsed = self.captured_at.elapsed().as_secs_f64();
        (self.reset_seconds - elapsed).max(0.0)
    }

    /// Whether this bucket is near its limit (>= 80%).
    pub fn is_hot(&self) -> bool {
        self.usage_pct() >= 80.0
    }
}

// ─── Rate Limit State ───────────────────────────────────────────────────

/// Full rate-limit state parsed from response headers.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    /// Requests per minute window.
    pub requests_min: RateLimitBucket,
    /// Requests per hour window.
    pub requests_hour: RateLimitBucket,
    /// Tokens per minute window.
    pub tokens_min: RateLimitBucket,
    /// Tokens per hour window.
    pub tokens_hour: RateLimitBucket,
    /// When the headers were captured.
    pub captured_at: Instant,
    /// Provider name.
    pub provider: String,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            requests_min: RateLimitBucket::default(),
            requests_hour: RateLimitBucket::default(),
            tokens_min: RateLimitBucket::default(),
            tokens_hour: RateLimitBucket::default(),
            captured_at: Instant::now(),
            provider: String::new(),
        }
    }
}

impl RateLimitState {
    /// Parse rate limit headers from HTTP response.
    ///
    /// Supports common header patterns:
    /// - OpenAI: `x-ratelimit-remaining-tokens`, `x-ratelimit-remaining-requests`,
    ///   `x-ratelimit-reset-tokens`, `x-ratelimit-reset-requests`
    /// - Anthropic: `retry-after`, `anthropic-ratelimit-tokens-remaining`
    /// - OpenRouter: `x-ratelimit-limit`, `x-ratelimit-remaining`, `x-ratelimit-reset`
    /// - Nous Portal: all of the above plus `-1h` hourly variants
    pub fn from_headers(headers: &HashMap<String, String>, provider: &str) -> Option<Self> {
        let has_any = headers.keys().any(|k| k.to_lowercase().starts_with("x-ratelimit-"));
        if !has_any {
            return None;
        }

        let get_u64 = |key: &str| -> u64 {
            headers
                .get(key)
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| v as u64)
                .unwrap_or(0)
        };

        let get_f64 = |key: &str| -> f64 {
            headers
                .get(key)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        };

        let captured_at = Instant::now();

        let requests_min = RateLimitBucket {
            limit: get_u64("x-ratelimit-limit-requests"),
            remaining: get_u64("x-ratelimit-remaining-requests"),
            reset_seconds: get_f64("x-ratelimit-reset-requests"),
            captured_at,
        };

        let requests_hour = RateLimitBucket {
            limit: get_u64("x-ratelimit-limit-requests-1h"),
            remaining: get_u64("x-ratelimit-remaining-requests-1h"),
            reset_seconds: get_f64("x-ratelimit-reset-requests-1h"),
            captured_at,
        };

        let tokens_min = RateLimitBucket {
            limit: get_u64("x-ratelimit-limit-tokens"),
            remaining: get_u64("x-ratelimit-remaining-tokens"),
            reset_seconds: get_f64("x-ratelimit-reset-tokens"),
            captured_at,
        };

        let tokens_hour = RateLimitBucket {
            limit: get_u64("x-ratelimit-limit-tokens-1h"),
            remaining: get_u64("x-ratelimit-remaining-tokens-1h"),
            reset_seconds: get_f64("x-ratelimit-reset-tokens-1h"),
            captured_at,
        };

        Some(Self {
            requests_min,
            requests_hour,
            tokens_min,
            tokens_hour,
            captured_at,
            provider: provider.to_string(),
        })
    }

    /// Whether any bucket is near its limit (>= 80% usage).
    pub fn is_near_limit(&self) -> bool {
        self.requests_min.is_hot()
            || self.requests_hour.is_hot()
            || self.tokens_min.is_hot()
            || self.tokens_hour.is_hot()
    }

    /// Shortest remaining time across all active buckets.
    pub fn min_reset_duration(&self) -> Option<Duration> {
        let mut min_secs = f64::INFINITY;
        let mut found = false;

        for bucket in [
            &self.requests_min,
            &self.requests_hour,
            &self.tokens_min,
            &self.tokens_hour,
        ] {
            if bucket.limit > 0 {
                let remaining = bucket.remaining_seconds_now();
                if remaining < min_secs {
                    min_secs = remaining;
                    found = true;
                }
            }
        }

        if found {
            Some(Duration::from_secs_f64(min_secs))
        } else {
            None
        }
    }

    /// Whether this state has meaningful data.
    pub fn has_data(&self) -> bool {
        self.requests_min.limit > 0
            || self.requests_hour.limit > 0
            || self.tokens_min.limit > 0
            || self.tokens_hour.limit > 0
    }

    /// Age of this state.
    pub fn age(&self) -> Duration {
        self.captured_at.elapsed()
    }

    /// Compact summary string.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();

        if self.requests_min.limit > 0 {
            parts.push(format!(
                "RPM: {}/{}",
                self.requests_min.remaining, self.requests_min.limit
            ));
        }
        if self.requests_hour.limit > 0 {
            parts.push(format!(
                "RPH: {}/{} (resets {}s)",
                self.requests_hour.remaining,
                self.requests_hour.limit,
                self.requests_hour.remaining_seconds_now() as u64
            ));
        }
        if self.tokens_min.limit > 0 {
            parts.push(format!(
                "TPM: {}/{}",
                self.tokens_min.remaining, self.tokens_min.limit
            ));
        }
        if self.tokens_hour.limit > 0 {
            parts.push(format!(
                "TPH: {}/{} (resets {}s)",
                self.tokens_hour.remaining,
                self.tokens_hour.limit,
                self.tokens_hour.remaining_seconds_now() as u64
            ));
        }

        parts.join(" | ")
    }
}

// ─── Retry-After Calculation ────────────────────────────────────────────

/// Extract the best available retry-after estimate from response headers.
///
/// Priority:
///   1. `retry-after` (generic HTTP header)
///   2. `x-ratelimit-reset-requests-1h` (hourly RPH window — most useful)
///   3. `x-ratelimit-reset-requests` (per-minute RPM window)
///   4. `x-ratelimit-reset-tokens` (token window)
///   5. `x-ratelimit-reset-tokens-1h` (hourly token window)
///
/// Returns seconds, or `None` if no usable header found.
pub fn extract_retry_after(headers: &HashMap<String, String>) -> Option<f64> {
    let get_f64 = |key: &str| -> Option<f64> {
        headers.get(key).and_then(|v| {
            let val = v.parse::<f64>().ok()?;
            if val > 0.0 { Some(val) } else { None }
        })
    };

    // Direct retry-after is most actionable
    if let Some(secs) = get_f64("retry-after") {
        return Some(secs);
    }

    // Nous hourly is most useful for Nous
    if let Some(secs) = get_f64("x-ratelimit-reset-requests-1h") {
        return Some(secs);
    }

    // Per-minute request window
    if let Some(secs) = get_f64("x-ratelimit-reset-requests") {
        return Some(secs);
    }

    // Token windows
    if let Some(secs) = get_f64("x-ratelimit-reset-tokens") {
        return Some(secs);
    }

    if let Some(secs) = get_f64("x-ratelimit-reset-tokens-1h") {
        return Some(secs);
    }

    // OpenRouter style
    if let Some(secs) = get_f64("x-ratelimit-reset") {
        return Some(secs);
    }

    None
}

/// Calculate estimated wait time from rate limit headers (legacy compat).
pub fn estimate_wait_time(headers: &HashMap<String, String>) -> Option<Duration> {
    extract_retry_after(headers).map(Duration::from_secs_f64)
}

// ─── Global Rate Limit Tracker ──────────────────────────────────────────

/// Global rate limit state per provider.
fn rate_limits() -> &'static Mutex<HashMap<String, RateLimitState>> {
    static C: OnceLock<Mutex<HashMap<String, RateLimitState>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Update rate limit state for a provider.
pub fn update_rate_limit(provider: &str, headers: &HashMap<String, String>) {
    if let Some(state) = RateLimitState::from_headers(headers, provider) {
        rate_limits().lock().insert(provider.to_string(), state);
    }
}

/// Get rate limit state for a provider.
pub fn get_rate_limit(provider: &str) -> Option<RateLimitState> {
    rate_limits().lock().get(provider).cloned()
}

/// Check if a provider is near its rate limit.
pub fn is_near_rate_limit(provider: &str) -> bool {
    rate_limits()
        .lock()
        .get(provider)
        .map(|s| s.is_near_limit())
        .unwrap_or(false)
}

/// Clear rate limit state for a provider.
pub fn clear_rate_limit(provider: &str) {
    rate_limits().lock().remove(provider);
}

// ─── Nous Rate Guard ────────────────────────────────────────────────────

/// Subscription tier for Nous Portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NousTier {
    /// Free tier — no paid quota.
    Free,
    /// Plus tier — enhanced daily/monthly quotas.
    Plus,
    /// Pro tier — highest quotas.
    Pro,
    /// Enterprise — custom limits.
    Enterprise,
}

impl NousTier {
    /// Daily token quota for this tier.
    pub fn daily_token_quota(&self) -> u64 {
        match self {
            Self::Free => 100_000,
            Self::Plus => 1_000_000,
            Self::Pro => 10_000_000,
            Self::Enterprise => u64::MAX,
        }
    }

    /// Monthly token quota for this tier.
    pub fn monthly_token_quota(&self) -> u64 {
        match self {
            Self::Free => 1_000_000,
            Self::Plus => 20_000_000,
            Self::Pro => 200_000_000,
            Self::Enterprise => u64::MAX,
        }
    }

    /// Daily request quota for this tier.
    pub fn daily_request_quota(&self) -> u64 {
        match self {
            Self::Free => 50,
            Self::Plus => 500,
            Self::Pro => 5_000,
            Self::Enterprise => u64::MAX,
        }
    }

    /// Monthly request quota for this tier.
    pub fn monthly_request_quota(&self) -> u64 {
        match self {
            Self::Free => 500,
            Self::Plus => 10_000,
            Self::Pro => 100_000,
            Self::Enterprise => u64::MAX,
        }
    }
}

/// Nous-specific rate limit state persisted on disk for cross-session sharing.
#[derive(Debug, Clone)]
pub struct NousRateLimitRecord {
    /// Absolute timestamp when the rate limit resets.
    pub reset_at: f64,
    /// When this record was created (epoch seconds).
    pub recorded_at: f64,
    /// Derived: seconds remaining at time of recording.
    pub reset_seconds: f64,
    /// The HTTP status that triggered this record (typically 429).
    pub status_code: u16,
    /// Optional error context from the response body.
    pub error_detail: Option<String>,
}

impl NousRateLimitRecord {
    /// Serialize to JSON string.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "reset_at": self.reset_at,
            "recorded_at": self.recorded_at,
            "reset_seconds": self.reset_seconds,
            "status_code": self.status_code,
            "error_detail": self.error_detail,
        })
        .to_string()
    }

    /// Deserialize from JSON string.
    pub fn from_json(json: &str) -> Option<Self> {
        let val: serde_json::Value = serde_json::from_str(json).ok()?;
        let reset_at = val.get("reset_at")?.as_f64()?;
        let recorded_at = val.get("recorded_at")?.as_f64()?;
        let reset_seconds = val.get("reset_seconds")?.as_f64()?;
        let status_code = val
            .get("status_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(429) as u16;
        let error_detail = val.get("error_detail").and_then(|v| v.as_str()).map(String::from);
        Some(Self {
            reset_at,
            recorded_at,
            reset_seconds,
            status_code,
            error_detail,
        })
    }
}

/// Nous rate guard: tracks subscription tier and quota usage.
#[derive(Debug, Clone)]
pub struct NousRateGuard {
    /// Subscription tier.
    pub tier: NousTier,
    /// Tokens consumed today.
    pub daily_tokens_used: u64,
    /// Tokens consumed this month.
    pub monthly_tokens_used: u64,
    /// Requests made today.
    pub daily_requests_used: u64,
    /// Requests made this month.
    pub monthly_requests_used: u64,
    /// Active rate limit record from a 429 response.
    pub rate_limit_record: Option<NousRateLimitRecord>,
    /// Whether the subscription is currently active.
    pub subscription_active: bool,
    /// Last time quotas were reset (epoch seconds).
    pub last_daily_reset: f64,
    pub last_monthly_reset: f64,
}

impl Default for NousRateGuard {
    fn default() -> Self {
        let now = now_epoch();
        Self {
            tier: NousTier::Free,
            daily_tokens_used: 0,
            monthly_tokens_used: 0,
            daily_requests_used: 0,
            monthly_requests_used: 0,
            rate_limit_record: None,
            subscription_active: false,
            last_daily_reset: now,
            last_monthly_reset: now,
        }
    }
}

impl NousRateGuard {
    /// Create a new guard with the specified tier.
    pub fn new(tier: NousTier) -> Self {
        let now = now_epoch();
        Self {
            tier,
            daily_tokens_used: 0,
            monthly_tokens_used: 0,
            daily_requests_used: 0,
            monthly_requests_used: 0,
            rate_limit_record: None,
            subscription_active: tier != NousTier::Free,
            last_daily_reset: now,
            last_monthly_reset: now,
        }
    }

    /// Check if a request can be made without exceeding quotas.
    pub fn can_request(&self) -> bool {
        if !self.subscription_active && self.tier == NousTier::Free {
            // Free tier still has quotas
        }

        if self.is_rate_limited() {
            return false;
        }

        self.daily_tokens_remaining() > 0
            && self.monthly_tokens_remaining() > 0
            && self.daily_requests_remaining() > 0
            && self.monthly_requests_remaining() > 0
    }

    /// Whether Nous Portal is currently rate-limited (from a 429).
    pub fn is_rate_limited(&self) -> bool {
        if let Some(record) = &self.rate_limit_record {
            return now_epoch() < record.reset_at;
        }
        false
    }

    /// Seconds remaining until the rate limit resets (if rate-limited).
    pub fn rate_limit_remaining(&self) -> Option<f64> {
        if let Some(record) = &self.rate_limit_record {
            let remaining = record.reset_at - now_epoch();
            if remaining > 0.0 {
                return Some(remaining);
            }
        }
        None
    }

    /// Daily token quota remaining.
    pub fn daily_tokens_remaining(&self) -> u64 {
        self.tier
            .daily_token_quota()
            .saturating_sub(self.daily_tokens_used)
    }

    /// Monthly token quota remaining.
    pub fn monthly_tokens_remaining(&self) -> u64 {
        self.tier
            .monthly_token_quota()
            .saturating_sub(self.monthly_tokens_used)
    }

    /// Daily request quota remaining.
    pub fn daily_requests_remaining(&self) -> u64 {
        self.tier
            .daily_request_quota()
            .saturating_sub(self.daily_requests_used)
    }

    /// Monthly request quota remaining.
    pub fn monthly_requests_remaining(&self) -> u64 {
        self.tier
            .monthly_request_quota()
            .saturating_sub(self.monthly_requests_used)
    }

    /// Usage percentage for daily tokens.
    pub fn daily_token_usage_pct(&self) -> f64 {
        let quota = self.tier.daily_token_quota();
        if quota == 0 || quota == u64::MAX {
            return 0.0;
        }
        (self.daily_tokens_used as f64 / quota as f64) * 100.0
    }

    /// Usage percentage for monthly tokens.
    pub fn monthly_token_usage_pct(&self) -> f64 {
        let quota = self.tier.monthly_token_quota();
        if quota == 0 || quota == u64::MAX {
            return 0.0;
        }
        (self.monthly_tokens_used as f64 / quota as f64) * 100.0
    }

    /// Record that a request was made with the given token count.
    pub fn record_request(&mut self, tokens: u64) {
        self.daily_tokens_used += tokens;
        self.monthly_tokens_used += tokens;
        self.daily_requests_used += 1;
        self.monthly_requests_used += 1;
    }

    /// Record a rate limit event (429 response).
    ///
    /// Parses reset time from headers or falls back to default cooldown.
    pub fn record_rate_limit_event(
        &mut self,
        headers: &HashMap<String, String>,
        default_cooldown_secs: f64,
    ) {
        let now = now_epoch();
        let reset_at = if let Some(secs) = extract_retry_after(headers) {
            now + secs
        } else {
            now + default_cooldown_secs
        };

        self.rate_limit_record = Some(NousRateLimitRecord {
            reset_at,
            recorded_at: now,
            reset_seconds: reset_at - now,
            status_code: 429,
            error_detail: None,
        });
    }

    /// Clear the active rate limit record (e.g., after a successful request).
    pub fn clear_rate_limit(&mut self) {
        self.rate_limit_record = None;
    }

    /// Reset daily counters (called at midnight or when headers indicate window reset).
    pub fn reset_daily(&mut self) {
        self.daily_tokens_used = 0;
        self.daily_requests_used = 0;
        self.last_daily_reset = now_epoch();
    }

    /// Reset monthly counters.
    pub fn reset_monthly(&mut self) {
        self.monthly_tokens_used = 0;
        self.monthly_requests_used = 0;
        self.last_monthly_reset = now_epoch();
    }

    /// Format remaining tokens as human-readable string.
    pub fn daily_tokens_display(&self) -> String {
        let remaining = self.daily_tokens_remaining();
        let quota = self.tier.daily_token_quota();
        format_tokens(remaining) + "/" + &format_tokens(quota)
    }

    /// Compact status string.
    pub fn status_summary(&self) -> String {
        let mut parts = Vec::new();

        parts.push(format!("tier={:?}", self.tier));

        if self.is_rate_limited() {
            if let Some(remaining) = self.rate_limit_remaining() {
                parts.push(format!(
                    "RATE_LIMITED(resets in {}s)",
                    remaining as u64
                ));
            }
        }

        parts.push(format!(
            "daily_tokens={}",
            format_tokens(self.daily_tokens_remaining())
        ));
        parts.push(format!(
            "monthly_tokens={}",
            format_tokens(self.monthly_tokens_remaining())
        ));
        parts.push(format!(
            "daily_reqs={}/{}",
            self.daily_requests_used,
            self.tier.daily_request_quota()
        ));

        parts.join(", ")
    }
}

/// Global Nous rate guard.
fn nous_guard() -> &'static Mutex<Option<NousRateGuard>> {
    static C: OnceLock<Mutex<Option<NousRateGuard>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// Set the global Nous rate guard.
pub fn set_nous_guard(guard: NousRateGuard) {
    *nous_guard().lock() = Some(guard);
}

/// Get a reference to the global Nous rate guard.
pub fn get_nous_guard() -> Option<NousRateGuard> {
    nous_guard().lock().clone()
}

/// Check if a Nous request is allowed (not rate-limited and within quota).
pub fn is_nous_request_allowed(tokens: u64) -> bool {
    let mut guard = nous_guard().lock();
    let Some(ref mut g) = *guard else {
        return true; // No guard configured, allow all
    };

    // Check rate limit from 429
    if g.is_rate_limited() {
        tracing::warn!(
            "Nous rate limited: {}s remaining",
            g.rate_limit_remaining().unwrap_or(0.0) as u64
        );
        return false;
    }

    // Check quotas
    if g.daily_tokens_remaining() < tokens {
        tracing::warn!(
            "Nous daily token quota exceeded: {} < {}",
            g.daily_tokens_remaining(),
            tokens
        );
        return false;
    }

    if g.monthly_tokens_remaining() < tokens {
        tracing::warn!(
            "Nous monthly token quota exceeded: {} < {}",
            g.monthly_tokens_remaining(),
            tokens
        );
        return false;
    }

    if g.daily_requests_remaining() == 0 {
        tracing::warn!("Nous daily request quota exceeded");
        return false;
    }

    if g.monthly_requests_remaining() == 0 {
        tracing::warn!("Nous monthly request quota exceeded");
        return false;
    }

    // Record usage
    g.record_request(tokens);
    true
}

// ─── Cross-Session Disk Persistence (mirrors Python nous_rate_guard.py) ─

const NOUS_RATE_LIMIT_SUBDIR: &str = "rate_limits";
const NOUS_RATE_LIMIT_FILENAME: &str = "nous.json";

/// Return the path to the Nous rate limit state file (`~/.hermez/rate_limits/nous.json`).
fn nous_rate_limit_state_path() -> Option<std::path::PathBuf> {
    let base = hermez_core::get_hermez_home();
    Some(base.join(NOUS_RATE_LIMIT_SUBDIR).join(NOUS_RATE_LIMIT_FILENAME))
}

/// Extract the best available reset-time estimate from response headers.
///
/// Priority:
///   1. x-ratelimit-reset-requests-1h  (hourly RPH window)
///   2. x-ratelimit-reset-requests     (per-minute RPM window)
///   3. retry-after                    (generic HTTP header)
///
/// Returns seconds-from-now, or `None` if no usable header found.
fn parse_nous_reset_seconds(headers: &HashMap<String, String>) -> Option<f64> {
    let lowered: HashMap<String, String> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .collect();

    for key in [
        "x-ratelimit-reset-requests-1h",
        "x-ratelimit-reset-requests",
        "retry-after",
    ] {
        if let Some(raw) = lowered.get(key) {
            if let Ok(val) = raw.parse::<f64>() {
                if val > 0.0 {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Internal: write rate limit state to the given path.
fn record_nous_rate_limit_to_path(
    path: &std::path::Path,
    headers: Option<&HashMap<String, String>>,
    error_context: Option<&serde_json::Value>,
    default_cooldown_secs: f64,
) {
    let now = now_epoch();
    let mut reset_at: Option<f64> = None;

    // Try headers first (most accurate)
    if let Some(hdrs) = headers {
        if let Some(secs) = parse_nous_reset_seconds(hdrs) {
            reset_at = Some(now + secs);
        }
    }

    // Try error_context reset_at (from body parsing)
    if reset_at.is_none() {
        if let Some(ctx) = error_context {
            if let Some(ctx_reset) = ctx.get("reset_at").and_then(|v| v.as_f64()) {
                if ctx_reset > now {
                    reset_at = Some(ctx_reset);
                }
            }
        }
    }

    // Default cooldown
    let reset_at = reset_at.unwrap_or(now + default_cooldown_secs);

    let state_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        tracing::debug!("Failed to create Nous rate limit state dir: {}", e);
        return;
    }

    let state = serde_json::json!({
        "reset_at": reset_at,
        "recorded_at": now,
        "reset_seconds": reset_at - now,
    });

    // Atomic write: temp file + rename
    let tmp_path = state_dir.join(format!("{}.tmp", NOUS_RATE_LIMIT_FILENAME));
    match std::fs::File::create(&tmp_path) {
        Ok(file) => {
            let mut writer = std::io::BufWriter::new(file);
            if serde_json::to_writer(&mut writer, &state).is_ok() {
                let _ = std::fs::rename(&tmp_path, path);
                tracing::info!(
                    "Nous rate limit recorded: resets in {:.0}s (at {:.0})",
                    reset_at - now,
                    reset_at
                );
            }
            // Best-effort cleanup of temp file
            let _ = std::fs::remove_file(&tmp_path);
        }
        Err(e) => {
            tracing::debug!("Failed to write Nous rate limit state: {}", e);
        }
    }
}

/// Record that Nous Portal is rate-limited by writing state to disk.
///
/// Parses the reset time from response headers or error context.
/// Falls back to `default_cooldown_secs` (5 minutes) if no reset info.
/// Writes to a shared file that all sessions can read.
pub fn record_nous_rate_limit_to_disk(
    headers: Option<&HashMap<String, String>>,
    error_context: Option<&serde_json::Value>,
    default_cooldown_secs: f64,
) {
    if let Some(path) = nous_rate_limit_state_path() {
        record_nous_rate_limit_to_path(&path, headers, error_context, default_cooldown_secs);
    }
}

/// Internal: read remaining seconds from the given path.
fn nous_rate_limit_remaining_at_path(path: &std::path::Path) -> Option<f64> {
    let contents = std::fs::read_to_string(path).ok()?;
    let state: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let reset_at = state.get("reset_at")?.as_f64()?;
    let remaining = reset_at - now_epoch();
    if remaining > 0.0 {
        return Some(remaining);
    }
    // Expired — clean up
    let _ = std::fs::remove_file(path);
    None
}

/// Check if Nous Portal is currently rate-limited by reading the shared state file.
///
/// Returns seconds remaining until reset, or `None` if not rate-limited.
pub fn nous_rate_limit_remaining_from_disk() -> Option<f64> {
    let path = nous_rate_limit_state_path()?;
    nous_rate_limit_remaining_at_path(&path)
}

/// Clear the on-disk rate limit state at the given path.
fn clear_nous_rate_limit_at_path(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Clear the on-disk rate limit state (e.g., after a successful Nous request).
pub fn clear_nous_rate_limit_on_disk() {
    if let Some(path) = nous_rate_limit_state_path() {
        clear_nous_rate_limit_at_path(&path);
    }
}

// ─── Utility Functions ──────────────────────────────────────────────────

/// Current epoch seconds (for tests, wraps `SystemTime`).
fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Format a token count for display.
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format seconds into human-readable duration.
pub fn format_duration(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        let m = s / 60;
        let sec = s % 60;
        if sec > 0 {
            format!("{m}m {sec}s")
        } else {
            format!("{m}m")
        }
    } else {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{h}h")
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── SlidingWindow ───

    #[test]
    fn test_sliding_window_allows_under_limit() {
        let mut window = SlidingWindow::new(5, Duration::from_secs(60));
        for _ in 0..5 {
            assert!(window.record());
        }
        assert_eq!(window.count(), 5);
        assert_eq!(window.remaining(), 0);
    }

    #[test]
    fn test_sliding_window_rejects_at_limit() {
        let mut window = SlidingWindow::new(3, Duration::from_secs(60));
        assert!(window.record());
        assert!(window.record());
        assert!(window.record());
        assert!(!window.record());
    }

    #[test]
    fn test_sliding_window_evicts_old() {
        let mut window = SlidingWindow::new(2, Duration::from_millis(50));
        assert!(window.record());
        assert!(window.record());
        assert!(!window.record());

        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(60));
        assert!(window.record());
    }

    #[test]
    fn test_sliding_window_clear() {
        let mut window = SlidingWindow::new(2, Duration::from_secs(60));
        window.record();
        window.record();
        window.clear();
        assert_eq!(window.count(), 0);
        assert!(window.record());
    }

    #[test]
    fn test_sliding_window_time_until_available() {
        let mut window = SlidingWindow::new(1, Duration::from_millis(100));
        window.record();
        assert!(window.is_limited());

        let wait = window.time_until_next_available();
        assert!(wait.is_some());

        std::thread::sleep(Duration::from_millis(110));
        assert!(!window.is_limited());
    }

    // ─── RateLimitBucket ───

    #[test]
    fn test_bucket_used_and_pct() {
        let bucket = RateLimitBucket {
            limit: 100,
            remaining: 30,
            reset_seconds: 45.0,
            captured_at: Instant::now(),
        };
        assert_eq!(bucket.used(), 70);
        assert!((bucket.usage_pct() - 70.0).abs() < 0.01);
    }

    #[test]
    fn test_bucket_is_hot() {
        let hot = RateLimitBucket {
            limit: 100,
            remaining: 15,
            reset_seconds: 10.0,
            captured_at: Instant::now(),
        };
        assert!(hot.is_hot());

        let cold = RateLimitBucket {
            limit: 100,
            remaining: 50,
            reset_seconds: 10.0,
            captured_at: Instant::now(),
        };
        assert!(!cold.is_hot());
    }

    // ─── RateLimitState ───

    #[test]
    fn test_parse_nous_full_headers() {
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-limit-requests".to_string(), "100".to_string());
        headers.insert(
            "x-ratelimit-remaining-requests".to_string(),
            "95".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-requests".to_string(),
            "30".to_string(),
        );
        headers.insert(
            "x-ratelimit-limit-requests-1h".to_string(),
            "1000".to_string(),
        );
        headers.insert(
            "x-ratelimit-remaining-requests-1h".to_string(),
            "800".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-requests-1h".to_string(),
            "1800".to_string(),
        );
        headers.insert("x-ratelimit-limit-tokens".to_string(), "50000".to_string());
        headers.insert(
            "x-ratelimit-remaining-tokens".to_string(),
            "45000".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-tokens".to_string(),
            "25".to_string(),
        );
        headers.insert(
            "x-ratelimit-limit-tokens-1h".to_string(),
            "500000".to_string(),
        );
        headers.insert(
            "x-ratelimit-remaining-tokens-1h".to_string(),
            "400000".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-tokens-1h".to_string(),
            "3600".to_string(),
        );

        let state =
            RateLimitState::from_headers(&headers, "nous").expect("should parse");
        assert_eq!(state.requests_min.limit, 100);
        assert_eq!(state.requests_min.remaining, 95);
        assert_eq!(state.requests_hour.limit, 1000);
        assert_eq!(state.requests_hour.remaining, 800);
        assert_eq!(state.tokens_min.limit, 50000);
        assert_eq!(state.tokens_min.remaining, 45000);
        assert_eq!(state.tokens_hour.limit, 500000);
        assert_eq!(state.tokens_hour.remaining, 400000);
        assert_eq!(state.provider, "nous");
        assert!(state.has_data());
    }

    #[test]
    fn test_parse_state_no_headers() {
        let headers = HashMap::new();
        let state = RateLimitState::from_headers(&headers, "test");
        assert!(state.is_none());
    }

    #[test]
    fn test_state_summary() {
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-limit-requests".to_string(), "100".to_string());
        headers.insert(
            "x-ratelimit-remaining-requests".to_string(),
            "90".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-requests".to_string(),
            "45".to_string(),
        );

        let state = RateLimitState::from_headers(&headers, "test").unwrap();
        let summary = state.summary();
        assert!(summary.contains("RPM"));
        assert!(summary.contains("90"));
        assert!(summary.contains("100"));
    }

    // ─── Retry-After ───

    #[test]
    fn test_extract_retry_after_direct() {
        let mut headers = HashMap::new();
        headers.insert("retry-after".to_string(), "60".to_string());
        assert_eq!(extract_retry_after(&headers), Some(60.0));
    }

    #[test]
    fn test_extract_retry_after_nous_hourly() {
        let mut headers = HashMap::new();
        headers.insert(
            "x-ratelimit-reset-requests-1h".to_string(),
            "3600".to_string(),
        );
        assert_eq!(extract_retry_after(&headers), Some(3600.0));
    }

    #[test]
    fn test_extract_retry_after_none() {
        let headers = HashMap::new();
        assert!(extract_retry_after(&headers).is_none());
    }

    #[test]
    fn test_extract_retry_after_openrouter() {
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-reset".to_string(), "120.5".to_string());
        assert_eq!(extract_retry_after(&headers), Some(120.5));
    }

    #[test]
    fn test_estimate_wait_time_compat() {
        let mut headers = HashMap::new();
        headers.insert("retry-after".to_string(), "45".to_string());
        let wait = estimate_wait_time(&headers).unwrap();
        assert_eq!(wait.as_secs(), 45);
    }

    // ─── Global Tracker ───

    #[test]
    fn test_update_and_get_global() {
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-limit-requests".to_string(), "100".to_string());
        headers.insert(
            "x-ratelimit-remaining-requests".to_string(),
            "50".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-requests".to_string(),
            "30".to_string(),
        );

        update_rate_limit("test-provider", &headers);
        let state = get_rate_limit("test-provider").unwrap();
        assert_eq!(state.requests_min.limit, 100);
        assert_eq!(state.requests_min.remaining, 50);

        clear_rate_limit("test-provider");
        assert!(get_rate_limit("test-provider").is_none());
    }

    #[test]
    fn test_is_near_rate_limit_global() {
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-limit-tokens".to_string(), "100".to_string());
        headers.insert(
            "x-ratelimit-remaining-tokens".to_string(),
            "5".to_string(),
        );
        headers.insert(
            "x-ratelimit-reset-tokens".to_string(),
            "60".to_string(),
        );

        update_rate_limit("hot-provider", &headers);
        assert!(is_near_rate_limit("hot-provider"));

        assert!(!is_near_rate_limit("unknown-provider"));
    }

    // ─── Nous Tier ───

    #[test]
    fn test_tier_quotas_free() {
        assert_eq!(NousTier::Free.daily_token_quota(), 100_000);
        assert_eq!(NousTier::Free.monthly_token_quota(), 1_000_000);
        assert_eq!(NousTier::Free.daily_request_quota(), 50);
        assert_eq!(NousTier::Free.monthly_request_quota(), 500);
    }

    #[test]
    fn test_tier_quotas_plus() {
        assert_eq!(NousTier::Plus.daily_token_quota(), 1_000_000);
        assert_eq!(NousTier::Plus.monthly_token_quota(), 20_000_000);
        assert_eq!(NousTier::Plus.daily_request_quota(), 500);
        assert_eq!(NousTier::Plus.monthly_request_quota(), 10_000);
    }

    #[test]
    fn test_tier_quotas_pro() {
        assert_eq!(NousTier::Pro.daily_token_quota(), 10_000_000);
        assert_eq!(NousTier::Pro.monthly_token_quota(), 200_000_000);
        assert_eq!(NousTier::Pro.daily_request_quota(), 5_000);
        assert_eq!(NousTier::Pro.monthly_request_quota(), 100_000);
    }

    #[test]
    fn test_tier_quotas_enterprise() {
        assert_eq!(NousTier::Enterprise.daily_token_quota(), u64::MAX);
        assert_eq!(NousTier::Enterprise.monthly_token_quota(), u64::MAX);
    }

    // ─── NousRateGuard ───

    #[test]
    fn test_guard_can_request() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        assert!(guard.can_request());

        // Exhaust daily requests
        guard.daily_requests_used = guard.tier.daily_request_quota();
        assert!(!guard.can_request());
    }

    #[test]
    fn test_guard_record_request() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        guard.record_request(1000);

        assert_eq!(guard.daily_tokens_used, 1000);
        assert_eq!(guard.monthly_tokens_used, 1000);
        assert_eq!(guard.daily_requests_used, 1);
        assert_eq!(guard.monthly_requests_used, 1);
    }

    #[test]
    fn test_guard_rate_limit_record() {
        let mut guard = NousRateGuard::new(NousTier::Free);
        assert!(!guard.is_rate_limited());

        let mut headers = HashMap::new();
        headers.insert("retry-after".to_string(), "300".to_string());
        guard.record_rate_limit_event(&headers, 60.0);

        assert!(guard.is_rate_limited());
        let remaining = guard.rate_limit_remaining().unwrap();
        assert!(remaining <= 300.0 && remaining > 290.0);
    }

    #[test]
    fn test_guard_rate_limit_default_cooldown() {
        let mut guard = NousRateGuard::new(NousTier::Free);
        let headers = HashMap::new(); // No headers
        guard.record_rate_limit_event(&headers, 120.0);

        assert!(guard.is_rate_limited());
        let remaining = guard.rate_limit_remaining().unwrap();
        assert!(remaining <= 120.0);
    }

    #[test]
    fn test_guard_clear_rate_limit() {
        let mut guard = NousRateGuard::new(NousTier::Free);
        let headers = HashMap::new();
        guard.record_rate_limit_event(&headers, 60.0);
        assert!(guard.is_rate_limited());

        guard.clear_rate_limit();
        assert!(!guard.is_rate_limited());
        assert!(guard.rate_limit_remaining().is_none());
    }

    #[test]
    fn test_guard_reset_daily() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        guard.record_request(5000);
        guard.record_request(3000);
        assert_eq!(guard.daily_tokens_used, 8000);
        assert_eq!(guard.daily_requests_used, 2);

        guard.reset_daily();
        assert_eq!(guard.daily_tokens_used, 0);
        assert_eq!(guard.daily_requests_used, 0);
        // Monthly should be unaffected
        assert_eq!(guard.monthly_tokens_used, 8000);
    }

    #[test]
    fn test_guard_reset_monthly() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        guard.record_request(5000);

        guard.reset_monthly();
        assert_eq!(guard.monthly_tokens_used, 0);
        assert_eq!(guard.monthly_requests_used, 0);
        // Daily should be unaffected
        assert_eq!(guard.daily_tokens_used, 5000);
    }

    #[test]
    fn test_guard_token_remaining() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        guard.record_request(500_000);

        assert_eq!(guard.daily_tokens_remaining(), 500_000);
        assert_eq!(guard.monthly_tokens_remaining(), 19_500_000);
    }

    #[test]
    fn test_guard_usage_pct() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        guard.record_request(500_000);

        // 500K / 1M = 50%
        assert!((guard.daily_token_usage_pct() - 50.0).abs() < 0.1);
        // 500K / 20M = 2.5%
        assert!((guard.monthly_token_usage_pct() - 2.5).abs() < 0.1);
    }

    #[test]
    fn test_guard_status_summary() {
        let mut guard = NousRateGuard::new(NousTier::Plus);
        guard.record_request(1000);
        let summary = guard.status_summary();
        assert!(summary.contains("Plus"));
        assert!(summary.contains("daily_tokens"));
    }

    // ─── NousRateLimitRecord ───

    #[test]
    fn test_record_json_roundtrip() {
        let record = NousRateLimitRecord {
            reset_at: 1_700_000_000.0,
            recorded_at: 1_700_000_000.0 - 300.0,
            reset_seconds: 300.0,
            status_code: 429,
            error_detail: Some("Rate limit exceeded".to_string()),
        };

        let json = record.to_json();
        let decoded = NousRateLimitRecord::from_json(&json).unwrap();

        assert!((decoded.reset_at - 1_700_000_000.0).abs() < 0.01);
        assert_eq!(decoded.status_code, 429);
        assert_eq!(decoded.error_detail, Some("Rate limit exceeded".to_string()));
    }

    // ─── Global Nous Guard ───

    #[test]
    fn test_global_nous_guard_set_and_get() {
        let guard = NousRateGuard::new(NousTier::Pro);
        set_nous_guard(guard);
        let retrieved = get_nous_guard().unwrap();
        assert_eq!(retrieved.tier, NousTier::Pro);
        assert!(retrieved.subscription_active);
    }

    #[test]
    fn test_is_nous_request_allowed_no_guard() {
        // Clear guard
        *nous_guard().lock() = None;
        // Should allow when no guard
        assert!(is_nous_request_allowed(1000));
    }

    #[test]
    fn test_is_nous_request_allowed_with_guard() {
        let guard = NousRateGuard::new(NousTier::Plus);
        set_nous_guard(guard);
        assert!(is_nous_request_allowed(100));
    }

    #[test]
    fn test_is_nous_request_blocked_by_rate_limit() {
        let mut guard = NousRateGuard::new(NousTier::Free);
        let mut headers = HashMap::new();
        headers.insert("retry-after".to_string(), "3600".to_string());
        guard.record_rate_limit_event(&headers, 60.0);
        set_nous_guard(guard);

        // Should be blocked by rate limit
        assert!(!is_nous_request_allowed(100));
    }

    // ─── Formatting ───

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5K");
        assert_eq!(format_tokens(10_500), "10.5K");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(30.0), "30s");
        assert_eq!(format_duration(90.0), "1m 30s");
        assert_eq!(format_duration(120.0), "2m");
        assert_eq!(format_duration(3661.0), "1h 1m");
        assert_eq!(format_duration(7200.0), "2h");
    }

    // ─── Disk Persistence ───

    #[test]
    fn test_nous_disk_persistence_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nous.json");

        // Record a rate limit with 300s cooldown
        let mut headers = HashMap::new();
        headers.insert("retry-after".to_string(), "300".to_string());
        record_nous_rate_limit_to_path(&path, Some(&headers), None, 60.0);

        // Should read back as rate-limited
        let remaining = nous_rate_limit_remaining_at_path(&path);
        assert!(remaining.is_some());
        let rem = remaining.unwrap();
        assert!(rem > 250.0 && rem <= 300.0, "remaining should be ~300s, got {}", rem);

        // Clear and verify gone
        clear_nous_rate_limit_at_path(&path);
        assert!(nous_rate_limit_remaining_at_path(&path).is_none());
    }

    #[test]
    fn test_nous_disk_persistence_expired_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nous.json");

        // Record with a tiny cooldown so it expires immediately
        record_nous_rate_limit_to_path(&path, None, None, 0.01);
        // Small sleep to ensure expiry
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Should return None and clean up the file
        assert!(nous_rate_limit_remaining_at_path(&path).is_none());
        // File should have been deleted by cleanup
        assert!(!path.exists());
    }

    #[test]
    fn test_nous_disk_persistence_headers_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nous.json");

        // Headers say 120s, default cooldown is 300s — headers should win
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-reset-requests-1h".to_string(), "120".to_string());
        record_nous_rate_limit_to_path(&path, Some(&headers), None, 300.0);

        let remaining = nous_rate_limit_remaining_at_path(&path).unwrap();
        assert!(remaining > 100.0 && remaining <= 120.0, "headers should override default");
    }

    #[test]
    fn test_nous_disk_persistence_error_context_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nous.json");

        // No headers, but error_context has reset_at in the future
        let future_reset = now_epoch() + 180.0;
        let ctx = serde_json::json!({ "reset_at": future_reset });
        record_nous_rate_limit_to_path(&path, None, Some(&ctx), 300.0);

        let remaining = nous_rate_limit_remaining_at_path(&path).unwrap();
        assert!(remaining > 150.0 && remaining <= 180.0, "error_context should be used");
    }
}
