//! Browser session manager.
//!
//! Tracks active browser sessions per task_id, handles session creation
//! on first access, and provides cleanup hooks.
//! Mirrors the Python `_active_sessions` + `_get_session_info` pattern.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use super::providers::CloudSession;

/// Information about an active browser session.
#[derive(Debug, Clone)]
pub struct BrowserSessionInfo {
    /// Task ID that owns this session.
    pub task_id: String,
    /// Session name (used for agent-browser --session).
    pub session_name: String,
    /// Provider session ID (for cloud cleanup).
    pub provider_session_id: String,
    /// CDP URL (for cloud mode).
    pub cdp_url: Option<String>,
    /// User ID (for Camofox mode).
    pub camofox_user_id: Option<String>,
    /// Tab ID (for Camofox mode).
    pub camofox_tab_id: Option<String>,
    /// Session creation time.
    pub created_at: Instant,
    /// Last access time.
    pub last_accessed: Instant,
}

/// Thread-safe browser session tracker.
pub struct BrowserSessionManager {
    sessions: Arc<Mutex<std::collections::HashMap<String, BrowserSessionInfo>>>,
}

impl BrowserSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Get or create a session for a task_id.
    /// If the session exists, update last_accessed and return it.
    /// If not, return None — caller should create the session and call `register`.
    pub fn get_session(&self, task_id: &str) -> Option<BrowserSessionInfo> {
        let mut sessions = self.sessions.lock();
        let info = sessions.get_mut(task_id)?;
        info.last_accessed = Instant::now();
        Some(info.clone())
    }

    /// Register a new cloud browser session.
    pub fn register_cloud(&self, task_id: &str, session: &CloudSession) -> BrowserSessionInfo {
        let now = Instant::now();
        let info = BrowserSessionInfo {
            task_id: task_id.to_string(),
            session_name: session.session_name.clone(),
            provider_session_id: session.provider_session_id.clone(),
            cdp_url: session.cdp_url.clone(),
            camofox_user_id: None,
            camofox_tab_id: None,
            created_at: now,
            last_accessed: now,
        };
        self.sessions.lock().insert(task_id.to_string(), info.clone());
        info
    }

    /// Register a Camofox session.
    pub fn register_camofox(&self, task_id: &str, user_id: &str, tab_id: &str) -> BrowserSessionInfo {
        let now = Instant::now();
        let info = BrowserSessionInfo {
            task_id: task_id.to_string(),
            session_name: format!("camofox-{user_id}"),
            provider_session_id: format!("{user_id}:{tab_id}"),
            cdp_url: None,
            camofox_user_id: Some(user_id.to_string()),
            camofox_tab_id: Some(tab_id.to_string()),
            created_at: now,
            last_accessed: now,
        };
        self.sessions.lock().insert(task_id.to_string(), info.clone());
        info
    }

    /// Register a local agent-browser session.
    pub fn register_local(&self, task_id: &str, session_name: &str) -> BrowserSessionInfo {
        let now = Instant::now();
        let info = BrowserSessionInfo {
            task_id: task_id.to_string(),
            session_name: session_name.to_string(),
            provider_session_id: String::new(),
            cdp_url: None,
            camofox_user_id: None,
            camofox_tab_id: None,
            created_at: now,
            last_accessed: now,
        };
        self.sessions.lock().insert(task_id.to_string(), info.clone());
        info
    }

    /// Remove a session.
    pub fn remove_session(&self, task_id: &str) -> Option<BrowserSessionInfo> {
        self.sessions.lock().remove(task_id)
    }

    /// Get all active task IDs.
    pub fn active_task_ids(&self) -> Vec<String> {
        self.sessions.lock().keys().cloned().collect()
    }

    /// Get sessions idle longer than the given duration.
    pub fn idle_sessions(&self, timeout: std::time::Duration) -> Vec<BrowserSessionInfo> {
        let now = Instant::now();
        self.sessions.lock()
            .values()
            .filter(|s| now.duration_since(s.last_accessed) > timeout)
            .cloned()
            .collect()
    }

    /// Clear all sessions (for testing).
    #[cfg(test)]
    pub fn clear(&self) {
        self.sessions.lock().clear();
    }
}

impl Default for BrowserSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_register_and_get_cloud_session() {
        let mgr = BrowserSessionManager::new();
        let session = CloudSession {
            session_name: "hermes-test".to_string(),
            provider_session_id: "bb-123".to_string(),
            cdp_url: Some("ws://localhost:9222".to_string()),
            features: Default::default(),
        };
        mgr.register_cloud("task-1", &session);

        let info = mgr.get_session("task-1").unwrap();
        assert_eq!(info.session_name, "hermes-test");
        assert_eq!(info.cdp_url, Some("ws://localhost:9222".to_string()));
    }

    #[test]
    fn test_register_and_get_camofox_session() {
        let mgr = BrowserSessionManager::new();
        mgr.register_camofox("task-1", "user-abc", "tab-xyz");

        let info = mgr.get_session("task-1").unwrap();
        assert!(info.camofox_user_id.is_some());
        assert!(info.camofox_tab_id.is_some());
        assert_eq!(info.camofox_tab_id.as_deref(), Some("tab-xyz"));
    }

    #[test]
    fn test_register_and_get_local_session() {
        let mgr = BrowserSessionManager::new();
        mgr.register_local("task-1", "hermes-task-1");

        let info = mgr.get_session("task-1").unwrap();
        assert_eq!(info.session_name, "hermes-task-1");
        assert!(info.cdp_url.is_none());
    }

    #[test]
    fn test_get_nonexistent_session() {
        let mgr = BrowserSessionManager::new();
        assert!(mgr.get_session("nonexistent").is_none());
    }

    #[test]
    fn test_remove_session() {
        let mgr = BrowserSessionManager::new();
        mgr.register_local("task-1", "hermes-task-1");
        let removed = mgr.remove_session("task-1");
        assert!(removed.is_some());
        assert!(mgr.get_session("task-1").is_none());
    }

    #[test]
    fn test_active_task_ids() {
        let mgr = BrowserSessionManager::new();
        mgr.register_local("task-1", "s1");
        mgr.register_local("task-2", "s2");

        let ids = mgr.active_task_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"task-1".to_string()));
        assert!(ids.contains(&"task-2".to_string()));
    }

    #[test]
    fn test_idle_sessions() {
        let mgr = BrowserSessionManager::new();
        mgr.register_local("task-1", "s1");

        // No idle sessions immediately
        let idle = mgr.idle_sessions(Duration::from_secs(1));
        assert!(idle.is_empty());

        // Simulate idle by clearing and re-registering with old timestamp
        let mut sessions = mgr.sessions.lock();
        if let Some(info) = sessions.get_mut("task-1") {
            info.last_accessed = Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap();
        }
        drop(sessions);

        let idle = mgr.idle_sessions(Duration::from_secs(5));
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].task_id, "task-1");
    }
}
