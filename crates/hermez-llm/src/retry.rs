#![allow(dead_code)]
//! Retry logic with exponential backoff.
//!
//! Mirrors the Python retry patterns in `run_agent.py` with
//! jitter, max retries, and configurable backoff.

use std::time::Duration;

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: true,
        }
    }
}

/// Retry with exponential backoff and optional jitter.
///
/// Returns the result of the last attempt or the first non-error.
/// The `operation` closure receives the attempt number (0-based).
pub async fn retry_with_backoff<T, E, F>(
    config: &RetryConfig,
    mut operation: impl FnMut(u32) -> F,
) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Debug,
{
    let mut last_err = None;

    for attempt in 0..=config.max_retries {
        match operation(attempt).await {
            Ok(value) => return Ok(value),
            Err(e) => {
                last_err = Some(e);
                if attempt < config.max_retries {
                    let delay = backoff_delay(config, attempt);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    Err(last_err.unwrap())
}

/// Calculate backoff delay for a given attempt.
///
/// Uses exponential backoff with optional jitter:
/// delay = min(base_delay * 2^attempt + random_jitter, max_delay)
fn backoff_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let exp = config.base_delay.mul_f64(2.0f64.powi(attempt as i32));
    let base = exp.min(config.max_delay);

    if config.jitter {
        // Add up to 50% of base as jitter, then cap at max_delay
        let jitter_ms = (base.as_millis() as u64) / 2;
        let random_ms = fastrand::u64(0..jitter_ms.max(1));
        let total = base + Duration::from_millis(random_ms);
        total.min(config.max_delay)
    } else {
        base
    }
}

/// Retry state for tracking attempts across a session.
#[derive(Debug, Default)]
pub struct RetryState {
    pub total_retries: u32,
    pub total_successes: u32,
    pub consecutive_failures: u32,
}

impl RetryState {
    /// Record a successful attempt.
    pub fn record_success(&mut self) {
        self.total_successes += 1;
        self.consecutive_failures = 0;
    }

    /// Record a failed attempt.
    pub fn record_failure(&mut self) {
        self.total_retries += 1;
        self.consecutive_failures += 1;
    }

    /// Success rate as a fraction.
    pub fn success_rate(&self) -> f64 {
        let total = self.total_successes + self.total_retries;
        if total == 0 {
            return 1.0;
        }
        self.total_successes as f64 / total as f64
    }
}

/// Payment exhaustion fallback chain.
///
/// When a provider returns 402/billing error, iterates a list of
/// fallback providers skipping the failed one.
pub struct FallbackChain<'a> {
    providers: &'a [&'a str],
    failed: Option<&'a str>,
    index: usize,
}

impl<'a> FallbackChain<'a> {
    pub fn new(providers: &'a [&'a str], failed: Option<&'a str>) -> Self {
        Self {
            providers,
            failed,
            index: 0,
        }
    }
}

impl<'a> Iterator for FallbackChain<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.providers.len() {
            let provider = self.providers[self.index];
            self.index += 1;
            if self.failed != Some(provider) {
                return Some(provider);
            }
        }
        None
    }
}

/// Credential-aware retry wrapper.
///
/// Mirrors Python `_recover_with_credential_pool` (run_agent.py:4856-4938).
/// On errors, classifies the failure and may rotate credentials before retrying.
///
/// Rotation rules:
/// - **Billing (402)**: Immediately rotate and retry
/// - **Rate limit (429)**: First occurrence does NOT rotate; second consecutive failure rotates
/// - **Auth (401)**: Try refresh first, then rotate if refresh fails
pub async fn call_with_credential_pool<F, Fut, T>(
    pool: &crate::credential_pool::CredentialPool,
    max_retries: u32,
    mut make_request: F,
) -> std::result::Result<T, crate::error_classifier::ClassifiedError>
where
    F: FnMut(&crate::credential_pool::Credential) -> Fut,
    Fut: std::future::Future<
        Output = std::result::Result<T, crate::error_classifier::ClassifiedError>,
    >,
{
    let mut consecutive_429 = false;
    let mut last_err: Option<crate::error_classifier::ClassifiedError> = None;

    for attempt in 0..=max_retries {
        let cred = match pool.current().or_else(|| pool.first()) {
            Some(c) => c,
            None => {
                // No credentials available — try with a default empty credential
                return make_request(&crate::credential_pool::Credential::new(String::new()))
                    .await;
            }
        };

        match make_request(&cred).await {
            Ok(value) => return Ok(value),
            Err(classified) => {
                use crate::error_classifier::FailoverReason;
                last_err = Some(classified.clone());

                match classified.reason {
                    FailoverReason::Billing => {
                        // Immediately rotate and retry
                        if pool.mark_exhausted_and_rotate(Some(402), None).is_some() {
                            consecutive_429 = false;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    }
                    FailoverReason::RateLimit => {
                        if consecutive_429 {
                            // Second consecutive 429 — rotate
                            if pool.mark_exhausted_and_rotate(Some(429), None).is_some() {
                                consecutive_429 = false;
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                continue;
                            }
                        } else {
                            // First 429 — don't rotate, set flag and retry
                            consecutive_429 = true;
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        }
                    }
                    FailoverReason::Auth => {
                        // Try refresh first
                        if pool.try_refresh_current().await {
                            // Refresh succeeded, retry with same credential
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                        // Refresh failed — rotate
                        if pool.mark_exhausted_and_rotate(Some(401), None).is_some() {
                            consecutive_429 = false;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    }
                    _ => {
                        // Other errors: apply normal retry with backoff
                        if attempt < max_retries {
                            let delay = backoff_delay(
                                &RetryConfig::default(),
                                attempt,
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                    }
                }

                // No rotation available or max retries reached
                return Err(classified);
            }
        }
    }

    Err(last_err.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_delay_increases() {
        let config = RetryConfig::default();
        let d0 = backoff_delay(&config, 0);
        let d1 = backoff_delay(&config, 1);
        let d2 = backoff_delay(&config, 2);
        assert!(d0 < d1);
        assert!(d1 < d2);
    }

    #[test]
    fn test_backoff_delay_respects_max() {
        let config = RetryConfig {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(2),
            ..Default::default()
        };
        // Attempt 10 should still be capped at max_delay (plus jitter)
        let delay = backoff_delay(&config, 10);
        // Without jitter this would be 1024s, with jitter it should still be near max
        assert!(delay <= config.max_delay * 2);
    }

    #[test]
    fn test_retry_state() {
        let mut state = RetryState::default();
        state.record_success();
        state.record_success();
        state.record_failure();
        state.record_success();
        state.record_failure();

        assert_eq!(state.total_successes, 3);
        assert_eq!(state.total_retries, 2);
        assert_eq!(state.consecutive_failures, 1);
        assert!((state.success_rate() - 0.6).abs() < 0.001);
    }

    #[test]
    fn test_retry_state_success_rate_empty() {
        let state = RetryState::default();
        assert_eq!(state.success_rate(), 1.0);
    }

    #[tokio::test]
    async fn test_retry_succeeds_on_first() {
        let config = RetryConfig::default();
        let result = retry_with_backoff(&config, |_attempt| async { Ok::<_, ()>("done") }).await;
        assert_eq!(result, Ok("done"));
    }

    #[tokio::test]
    async fn test_retry_eventually_fails() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(1), // fast for tests
            ..Default::default()
        };
        let result = retry_with_backoff(&config, |_attempt| async {
            Err::<(), _>("fail")
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_fallback_chain() {
        let providers = ["openrouter", "nous", "custom", "codex"];
        let mut chain = FallbackChain::new(&providers, Some("openrouter"));
        assert_eq!(chain.next(), Some("nous"));
        assert_eq!(chain.next(), Some("custom"));
        assert_eq!(chain.next(), Some("codex"));
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn test_fallback_chain_no_failure() {
        let providers = ["openrouter", "nous"];
        let mut chain = FallbackChain::new(&providers, None);
        assert_eq!(chain.next(), Some("openrouter"));
        assert_eq!(chain.next(), Some("nous"));
        assert_eq!(chain.next(), None);
    }
}
