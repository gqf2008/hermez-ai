//! Browser provider router.
//!
//! Selects the appropriate browser backend based on environment variables
//! and configuration. Mirrors the Python `_get_cloud_provider()` logic.
//!
//! Selection order:
//! 1. `BROWSER_CDP_URL` — direct CDP endpoint (bypasses all providers)
//! 2. `CAMOFOX_URL` — local Camofox anti-detection browser
//! 3. `browser.cloud_provider` config — explicit provider selection
//! 4. Auto-detect: BrowserUse → Browserbase → local

use std::sync::Arc;

use super::providers::{CloudBrowserProvider, browser_use::BrowserUseProvider, browserbase::BrowserbaseProvider, firecrawl::FirecrawlProvider};

/// Resolved browser backend.
pub enum BrowserBackend {
    /// Direct CDP URL — bypasses all providers.
    DirectCdp(String),
    /// Camofox local anti-detection browser.
    Camofox,
    /// Cloud provider (BrowserUse, Browserbase, Firecrawl).
    Cloud(Arc<dyn CloudBrowserProvider>),
    /// Local agent-browser CLI (no cloud provider).
    Local,
}

impl std::fmt::Display for BrowserBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserBackend::DirectCdp(url) => write!(f, "direct-cdp({url})"),
            BrowserBackend::Camofox => write!(f, "camofox"),
            BrowserBackend::Cloud(p) => write!(f, "cloud({})", p.provider_name()),
            BrowserBackend::Local => write!(f, "local"),
        }
    }
}

/// Resolve the browser backend from environment and config.
pub fn resolve_backend(config_provider: Option<&str>) -> BrowserBackend {
    // 1. Direct CDP URL override
    if let Ok(cdp_url) = std::env::var("BROWSER_CDP_URL") {
        if !cdp_url.is_empty() {
            tracing::info!("Browser: using direct CDP endpoint");
            return BrowserBackend::DirectCdp(cdp_url);
        }
    }

    // 2. Camofox mode
    if let Ok(camofox_url) = std::env::var("CAMOFOX_URL") {
        if !camofox_url.is_empty() {
            tracing::info!("Browser: using Camofox ({camofox_url})");
            return BrowserBackend::Camofox;
        }
    }

    // 3. Explicit provider from config
    if let Some(provider) = config_provider {
        match provider {
            "local" => {
                tracing::info!("Browser: forced to local mode by config");
                return BrowserBackend::Local;
            }
            "browserbase" => {
                let p = BrowserbaseProvider::from_env();
                if p.is_configured() {
                    tracing::info!("Browser: using Browserbase");
                    return BrowserBackend::Cloud(Arc::new(p));
                } else {
                    tracing::warn!("Browser: Browserbase configured but missing credentials");
                }
            }
            "browser-use" | "browser_use" => {
                let p = BrowserUseProvider::from_env();
                if p.is_configured() {
                    tracing::info!("Browser: using Browser Use");
                    return BrowserBackend::Cloud(Arc::new(p));
                }
            }
            "firecrawl" => {
                let p = FirecrawlProvider::from_env();
                if p.is_configured() {
                    tracing::info!("Browser: using Firecrawl");
                    return BrowserBackend::Cloud(Arc::new(p));
                }
            }
            other => {
                tracing::warn!("Browser: unknown provider '{other}', falling back to auto-detect");
            }
        }
    }

    // 4. Auto-detect chain
    // Try BrowserUse first (supports managed Nous gateway or direct key)
    let browser_use = BrowserUseProvider::from_env();
    if browser_use.is_configured() {
        tracing::info!("Browser: auto-detected Browser Use");
        return BrowserBackend::Cloud(Arc::new(browser_use));
    }

    // Try Browserbase (direct credentials only)
    let browserbase = BrowserbaseProvider::from_env();
    if browserbase.is_configured() {
        tracing::info!("Browser: auto-detected Browserbase");
        return BrowserBackend::Cloud(Arc::new(browserbase));
    }

    // Try Firecrawl
    let firecrawl = FirecrawlProvider::from_env();
    if firecrawl.is_configured() {
        tracing::info!("Browser: auto-detected Firecrawl");
        return BrowserBackend::Cloud(Arc::new(firecrawl));
    }

    // Fallback to local
    tracing::info!("Browser: no cloud provider configured, using local mode");
    BrowserBackend::Local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_local_default() {
        // Without any env vars, should resolve to Local
        let backend = resolve_backend(None);
        assert!(matches!(backend, BrowserBackend::Local));
    }

    #[test]
    fn test_resolve_explicit_local() {
        let backend = resolve_backend(Some("local"));
        assert!(matches!(backend, BrowserBackend::Local));
    }

    #[test]
    fn test_resolve_unknown_provider() {
        let backend = resolve_backend(Some("unknown-provider"));
        // Falls through to auto-detect → Local
        assert!(matches!(backend, BrowserBackend::Local));
    }

    #[test]
    fn test_display_backend() {
        assert_eq!(format!("{}", BrowserBackend::Local), "local");
        assert_eq!(format!("{}", BrowserBackend::Camofox), "camofox");
        assert!(format!("{}", BrowserBackend::DirectCdp("ws://x".to_string())).starts_with("direct-cdp"));
    }
}
