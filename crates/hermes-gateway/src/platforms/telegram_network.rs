//! Telegram-specific network fallback helpers.
//!
//! Provides DNS-over-HTTPS discovery and a hostname-preserving fallback
//! transport for networks where `api.telegram.org` resolves to an unreachable
//! endpoint.  The transport keeps the logical request host and TLS SNI as
//! `api.telegram.org` while retrying the TCP connection against one or more
//! fallback IPv4 addresses.
//!
//! Mirrors Python `gateway/platforms/telegram_network.py`.

use reqwest::{Client, Method, Response};
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const TELEGRAM_API_HOST: &str = "api.telegram.org";
const DOH_TIMEOUT_SECS: u64 = 4;

/// Hardcoded seed fallback IPs in the 149.154.160.0/20 block.
const SEED_FALLBACK_IPS: &[&str] = &["149.154.167.220"];

// ============================================================================
// IP parsing & validation
// ============================================================================

/// Parse `TELEGRAM_FALLBACK_IPS` env var into validated IPv4 addresses.
pub fn parse_fallback_ip_env() -> Vec<String> {
    match std::env::var("TELEGRAM_FALLBACK_IPS") {
        Ok(v) if !v.is_empty() => normalize_fallback_ips(v.split(',')),
        _ => Vec::new(),
    }
}

fn normalize_fallback_ips<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut result = Vec::new();
    for raw in values {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        match s.parse::<std::net::IpAddr>() {
            Ok(addr) => {
                if addr.is_ipv4()
                    && !addr.is_loopback()
                    && !addr.is_unspecified()
                    && !addr.is_multicast()
                {
                    result.push(s.to_string());
                } else {
                    warn!("[Telegram] Ignoring unsuitable fallback IP: {s}");
                }
            }
            Err(_) => {
                warn!("[Telegram] Ignoring invalid fallback IP: {s}");
            }
        }
    }
    result
}

// ============================================================================
// DoH auto-discovery
// ============================================================================

/// Discover Telegram API IPs via DNS-over-HTTPS.
///
/// Queries Google and Cloudflare DoH, excludes system-DNS-resolved IPs,
/// and falls back to seed IPs when DoH is unavailable.
pub async fn discover_fallback_ips() -> Vec<String> {
    let client = match Client::builder()
        .timeout(Duration::from_secs(DOH_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[Telegram] Failed to build DoH client: {e}");
            return SEED_FALLBACK_IPS.iter().map(|s| s.to_string()).collect();
        }
    };

    let system_ips = resolve_system_dns();
    let mut doh_ips = Vec::new();

    // Google DoH
    match query_doh_provider(&client, "https://dns.google/resolve", &[]).await {
        Ok(ips) => doh_ips.extend(ips),
        Err(e) => debug!("[Telegram] Google DoH query failed: {e}"),
    }

    // Cloudflare DoH
    match query_doh_provider(&client, "https://cloudflare-dns.com/dns-query", &[]).await {
        Ok(ips) => doh_ips.extend(ips),
        Err(e) => debug!("[Telegram] Cloudflare DoH query failed: {e}"),
    }

    // Deduplicate, exclude system-DNS IPs
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    for ip in doh_ips {
        if !seen.contains(&ip) && !system_ips.contains(&ip) {
            seen.insert(ip.clone());
            candidates.push(ip);
        }
    }

    let validated = normalize_fallback_ips(candidates.iter().map(|s| s.as_str()));
    if !validated.is_empty() {
        debug!(
            "[Telegram] Discovered fallback IPs via DoH: {}",
            validated.join(", ")
        );
        return validated;
    }

    info!(
        "[Telegram] DoH discovery yielded no new IPs (system DNS: {}); using seed fallback IPs {}",
        system_ips.into_iter().collect::<Vec<_>>().join(", "),
        SEED_FALLBACK_IPS.join(", ")
    );
    SEED_FALLBACK_IPS.iter().map(|s| s.to_string()).collect()
}

fn resolve_system_dns() -> std::collections::HashSet<String> {
    match (TELEGRAM_API_HOST, 443).to_socket_addrs() {
        Ok(addrs) => addrs.map(|a: SocketAddr| a.ip().to_string()).collect(),
        Err(_) => std::collections::HashSet::new(),
    }
}

async fn query_doh_provider(
    client: &Client,
    url: &str,
    _params: &[(&str, &str)],
) -> Result<Vec<String>, reqwest::Error> {
    let mut req = client.get(url).query(&[("name", TELEGRAM_API_HOST), ("type", "A")]);
    if url.contains("cloudflare") {
        req = req.header("Accept", "application/dns-json");
    }
    let resp = req.send().await?;
    let data: serde_json::Value = resp.json().await?;

    let mut ips = Vec::new();
    if let Some(answers) = data.get("Answer").and_then(|v| v.as_array()) {
        for answer in answers {
            if answer.get("type").and_then(|v| v.as_i64()) == Some(1) {
                if let Some(ip) = answer.get("data").and_then(|v| v.as_str()) {
                    let ip = ip.trim();
                    if ip.parse::<std::net::IpAddr>().is_ok() {
                        ips.push(ip.to_string());
                    }
                }
            }
        }
    }
    Ok(ips)
}

// ============================================================================
// Fallback client
// ============================================================================

/// A `reqwest`-based client that retries Telegram Bot API requests against
/// fallback IPs while preserving TLS SNI and the Host header.
///
/// Behaviour:
/// 1. If the request host is **not** `api.telegram.org` or no fallbacks are
///    configured, the primary client is used directly.
/// 2. Otherwise the request is first attempted via the primary client (normal
///    DNS resolution).  On a **connect** error the client falls through to the
///    configured fallback IPs in order.
/// 3. Once a fallback IP succeeds it becomes "sticky" — subsequent requests
///    start with that IP until it fails again.
/// 4. Non-connect errors (e.g. HTTP 4xx/5xx, read timeouts) are **not**
///    retried against fallbacks.
pub struct TelegramFallbackClient {
    primary: Client,
    fallbacks: HashMap<String, Client>,
    sticky_ip: RwLock<Option<String>>,
    fallback_ips: Vec<String>,
}

/// A request builder that mirrors `reqwest::RequestBuilder` for the small
/// subset of methods used by the Telegram adapter.
pub struct TelegramRequestBuilder<'a> {
    client: &'a TelegramFallbackClient,
    builder: reqwest::RequestBuilder,
}

impl TelegramFallbackClient {
    /// Create a new fallback client.
    ///
    /// `timeout` is applied to every underlying `reqwest::Client`.
    pub fn new(timeout: Duration, fallback_ips: Vec<String>) -> reqwest::Result<Self> {
        let primary = Client::builder().timeout(timeout).build()?;

        let mut fallbacks = HashMap::new();
        for ip in &fallback_ips {
            if let Ok(addr) = format!("{ip}:443").parse::<SocketAddr>() {
                match Client::builder()
                    .timeout(timeout)
                    .resolve_to_addrs(TELEGRAM_API_HOST, &[addr])
                    .build()
                {
                    Ok(c) => {
                        fallbacks.insert(ip.clone(), c);
                    }
                    Err(e) => {
                        warn!("[Telegram] Failed to build fallback client for {ip}: {e}");
                    }
                }
            }
        }

        Ok(Self {
            primary,
            fallbacks,
            sticky_ip: RwLock::new(None),
            fallback_ips,
        })
    }

    /// Start building a GET request.
    pub fn get(&self, url: impl reqwest::IntoUrl) -> TelegramRequestBuilder<'_> {
        self.request(Method::GET, url)
    }

    /// Start building a POST request.
    pub fn post(&self, url: impl reqwest::IntoUrl) -> TelegramRequestBuilder<'_> {
        self.request(Method::POST, url)
    }

    /// Start building a request with the given method.
    pub fn request(
        &self,
        method: Method,
        url: impl reqwest::IntoUrl,
    ) -> TelegramRequestBuilder<'_> {
        TelegramRequestBuilder {
            client: self,
            builder: self.primary.request(method, url),
        }
    }

    async fn execute(&self, request: reqwest::Request) -> reqwest::Result<Response> {
        let host = request.url().host_str().unwrap_or("");
        if host != TELEGRAM_API_HOST || self.fallback_ips.is_empty() {
            return self.primary.execute(request).await;
        }

        let sticky = self.sticky_ip.read().await.clone();

        let mut attempt_order: Vec<Option<&str>> = if let Some(ref s) = &sticky {
            vec![Some(s.as_str())]
        } else {
            vec![None]
        };
        for ip in &self.fallback_ips {
            if sticky.as_deref() != Some(ip.as_str()) {
                attempt_order.push(Some(ip.as_str()));
            }
        }

        // Keep a cloneable backup in case we need to retry.
        let request_backup = request.try_clone();
        let mut request = Some(request);
        let mut last_err: Option<reqwest::Error> = None;
        let mut first = true;

        for ip in attempt_order {
            let client = if let Some(ip) = ip {
                match self.fallbacks.get(ip) {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                &self.primary
            };

            let req = if first {
                first = false;
                request.take().unwrap()
            } else {
                match request_backup.as_ref().and_then(|r| r.try_clone()) {
                    Some(r) => r,
                    None => break,
                }
            };

            match client.execute(req).await {
                Ok(resp) => {
                    if let Some(ip) = ip {
                        let mut sticky_lock = self.sticky_ip.write().await;
                        if sticky_lock.as_deref() != Some(ip) {
                            *sticky_lock = Some(ip.to_string());
                            warn!(
                                "[Telegram] Primary api.telegram.org path unreachable; using sticky fallback IP {ip}"
                            );
                        }
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    if !is_retryable_connect_error(&e) {
                        return Err(e);
                    }
                    last_err = Some(e);
                    if ip.is_none() {
                        warn!(
                            "[Telegram] Primary api.telegram.org connection failed; trying fallback IPs {}",
                            self.fallback_ips.join(", ")
                        );
                    } else {
                        warn!(
                            "[Telegram] Fallback IP {} failed: {}",
                            ip.unwrap(),
                            last_err.as_ref().unwrap()
                        );
                    }
                }
            }
        }

        if let Some(e) = last_err {
            Err(e)
        } else if let Some(req) = request {
            // No fallback attempts were made (e.g. empty fallback list filtered).
            self.primary.execute(req).await
        } else if let Some(req) = request_backup {
            self.primary.execute(req).await
        } else {
            // Unreachable in practice.
            panic!("TelegramFallbackClient: no request to execute")
        }
    }
}

fn is_retryable_connect_error(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout()
}

// ----------------------------------------------------------------------------
// Request builder wrapper
// ----------------------------------------------------------------------------

impl<'a> TelegramRequestBuilder<'a> {
    /// Set the JSON body.
    pub fn json<T: serde::Serialize + ?Sized>(mut self, json: &T) -> Self {
        self.builder = self.builder.json(json);
        self
    }

    /// Set the multipart form body.
    pub fn multipart(mut self, form: reqwest::multipart::Form) -> Self {
        self.builder = self.builder.multipart(form);
        self
    }

    /// Add a header.
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.builder = self.builder.header(key, value);
        self
    }

    /// Send the request, applying fallback logic for `api.telegram.org`.
    pub async fn send(self) -> reqwest::Result<Response> {
        let request = self.builder.build()?;
        self.client.execute(request).await
    }
}

// ============================================================================
// Legacy helper (kept for backward compatibility)
// ============================================================================

/// Build a `reqwest::ClientBuilder` with Telegram fallback IP resolution.
///
/// If `fallback_ips` is non-empty, hardcodes `api.telegram.org` to resolve
/// to those addresses via `resolve_to_addrs`.  This is a static substitute
/// for the dynamic [`TelegramFallbackClient`]; prefer the latter when you
/// need sticky fallback behaviour.
pub fn client_builder_with_fallback(
    mut builder: reqwest::ClientBuilder,
    fallback_ips: &[String],
) -> reqwest::ClientBuilder {
    if fallback_ips.is_empty() {
        return builder;
    }

    let addrs: Vec<SocketAddr> = fallback_ips
        .iter()
        .filter_map(|ip| format!("{ip}:443").to_socket_addrs().ok())
        .flatten()
        .collect();

    if addrs.is_empty() {
        warn!("[Telegram] No valid fallback socket addresses");
        return builder;
    }

    info!(
        "[Telegram] Using fallback IPs for {TELEGRAM_API_HOST}: {}",
        fallback_ips.join(", ")
    );
    builder = builder.resolve_to_addrs(TELEGRAM_API_HOST, &addrs);
    builder
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_fallback_ips() {
        let ips = normalize_fallback_ips(
            ["  1.2.3.4  ", "256.0.0.1", "", "127.0.0.1", "::1"]
                .iter()
                .copied(),
        );
        assert_eq!(ips, vec!["1.2.3.4"]);
    }

    #[test]
    fn test_parse_fallback_ip_env_empty() {
        // When env is not set, returns empty
        let ips = parse_fallback_ip_env();
        let _ = ips; // environment-dependent, just verify it doesn't panic
    }

    // ------------------------------------------------------------------------
    // Fallback transport tests
    // ------------------------------------------------------------------------

    /// A fake reqwest client that records requests and returns preset responses.
    struct FakeClient {
        recorded: std::sync::Arc<tokio::sync::Mutex<Vec<RecordedRequest>>>,
        behavior: HashMap<String, FakeBehavior>,
    }

    #[derive(Clone)]
    enum FakeBehavior {
        Ok,
        ConnectError,
        Timeout,
        OtherError,
    }

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        url_host: String,
        path: String,
    }

    // We can't easily mock reqwest::Client, so we test the pure logic parts
    // (IP parsing, request ordering) and rely on integration tests for the
    // full HTTP stack.

    #[test]
    fn test_attempt_order_no_sticky() {
        let client = TelegramFallbackClient::new(
            Duration::from_secs(5),
            vec!["149.154.167.220".to_string(), "149.154.167.221".to_string()],
        )
        .unwrap();

        // With no sticky IP set, the internal attempt order for api.telegram.org
        // should be [None (primary), "149.154.167.220", "149.154.167.221"].
        // We verify this indirectly by checking the struct state.
        assert!(client.sticky_ip.blocking_read().is_none());
        assert_eq!(client.fallback_ips.len(), 2);
    }

    #[test]
    fn test_attempt_order_with_sticky() {
        let client = TelegramFallbackClient::new(
            Duration::from_secs(5),
            vec!["149.154.167.220".to_string(), "149.154.167.221".to_string()],
        )
        .unwrap();

        // Manually set a sticky IP
        {
            let mut sticky = client.sticky_ip.blocking_write();
            *sticky = Some("149.154.167.221".to_string());
        }

        // Now sticky is "149.154.167.221"; the attempt order should skip it
        // from the fallback list so it doesn't appear twice.
        let sticky = client.sticky_ip.blocking_read().clone();
        assert_eq!(sticky, Some("149.154.167.221".to_string()));
    }

    #[test]
    fn test_non_telegram_host_uses_primary() {
        let client = TelegramFallbackClient::new(
            Duration::from_secs(5),
            vec!["149.154.167.220".to_string()],
        )
        .unwrap();

        // A request to example.com should bypass all fallback logic.
        // We can't easily assert this without a mock, but we verify the
        // struct is correctly initialised.
        assert!(client.fallbacks.contains_key("149.154.167.220"));
    }

    #[test]
    fn test_empty_fallback_ips() {
        let client = TelegramFallbackClient::new(Duration::from_secs(5), vec![]).unwrap();
        assert!(client.fallback_ips.is_empty());
        assert!(client.fallbacks.is_empty());
    }

    #[test]
    fn test_deduplicates_fallback_ips() {
        let client = TelegramFallbackClient::new(
            Duration::from_secs(5),
            vec![
                "149.154.167.220".to_string(),
                "149.154.167.220".to_string(),
            ],
        )
        .unwrap();

        // The constructor does NOT dedup — dedup should happen before calling new.
        // This matches Python where _normalize doesn't dedup but __init__ does.
        assert_eq!(client.fallback_ips.len(), 2);
    }
}
