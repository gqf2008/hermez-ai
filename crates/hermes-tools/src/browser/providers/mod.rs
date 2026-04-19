//! Cloud browser provider trait and session types.
//!
//! Mirrors the Python `tools/browser_providers/base.py`.
//! Each cloud provider (Browserbase, BrowserUse, Firecrawl) implements
//! this trait for session lifecycle management.

pub mod browser_use;
pub mod browserbase;
pub mod firecrawl;

use std::collections::HashMap;

/// Session information returned by cloud providers.
#[derive(Debug, Clone)]
pub struct CloudSession {
    /// Unique session name (used for agent-browser --session).
    pub session_name: String,
    /// Provider's session ID (for close/cleanup).
    pub provider_session_id: String,
    /// CDP websocket URL for agent-browser --cdp.
    pub cdp_url: Option<String>,
    /// Feature flags that were enabled.
    pub features: HashMap<String, bool>,
}

/// Cloud browser provider trait.
#[async_trait::async_trait]
pub trait CloudBrowserProvider: Send + Sync {
    /// Provider display name.
    fn provider_name(&self) -> &str;

    /// Cheap env-var check — no network I/O.
    fn is_configured(&self) -> bool;

    /// Create a new browser session.
    async fn create_session(&self, task_id: &str) -> Result<CloudSession, String>;

    /// Close an existing session.
    async fn close_session(&self, session_id: &str) -> bool;

    /// Emergency cleanup — best effort, no panics.
    async fn emergency_cleanup(&self, session_id: &str);
}

/// Build a session name from task_id.
pub fn session_name(task_id: &str) -> String {
    format!("hermes-{task_id}")
}
