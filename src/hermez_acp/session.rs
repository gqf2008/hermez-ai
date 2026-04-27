//! Session management for ACP adapter.
//!
//! In-memory session storage with `parking_lot::RwLock` for thread safety.
//! Mirrors `acp_adapter/session.py` — without SQLite persistence (MVP scope).

#![allow(dead_code)]

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// MCP server configuration for ACP sessions.
#[derive(Debug, Clone)]
pub struct AcpMcpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
}

/// Running state for a single ACP session.
pub struct SessionState {
    pub session_id: String,
    pub cwd: String,
    /// Cancel signal for the running agent.
    pub cancelled: Arc<RwLock<bool>>,
    /// MCP servers to register for this session.
    /// Mirrors Python _register_session_mcp_servers() (acp_adapter/server.py).
    pub mcp_servers: Option<Vec<AcpMcpServerConfig>>,
}

/// Thread-safe session manager.
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<SessionState>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new session with the given working directory.
    pub fn create_session(&self, cwd: &str) -> Arc<SessionState> {
        let state = Arc::new(SessionState {
            session_id: Uuid::new_v4().to_string(),
            cwd: cwd.to_string(),
            cancelled: Arc::new(RwLock::new(false)),
            mcp_servers: None,
        });
        self.sessions
            .write()
            .insert(state.session_id.clone(), state.clone());
        state
    }

    /// Get an existing session by ID.
    pub fn get_session(&self, session_id: &str) -> Option<Arc<SessionState>> {
        self.sessions.read().get(session_id).cloned()
    }

    /// Remove a session by ID.
    pub fn remove_session(&self, session_id: &str) {
        self.sessions.write().remove(session_id);
    }

    /// Update the working directory for a session.
    pub fn update_cwd(&self, session_id: &str, cwd: &str) -> Option<Arc<SessionState>> {
        let mut sessions = self.sessions.write();
        if let Some(state) = sessions.get_mut(session_id) {
            // Create a new Arc with updated cwd
            let new_state = Arc::new(SessionState {
                session_id: state.session_id.clone(),
                cwd: cwd.to_string(),
                cancelled: state.cancelled.clone(),
                mcp_servers: state.mcp_servers.clone(),
            });
            sessions.insert(session_id.to_string(), new_state.clone());
            Some(new_state)
        } else {
            None
        }
    }

    /// Fork an existing session into a new one.
    pub fn fork_session(&self, session_id: &str, cwd: &str) -> Option<Arc<SessionState>> {
        let sessions = self.sessions.read();
        let _original = sessions.get(session_id)?;
        let new_state = Arc::new(SessionState {
            session_id: Uuid::new_v4().to_string(),
            cwd: cwd.to_string(),
            cancelled: Arc::new(RwLock::new(false)),
            mcp_servers: None,
        });
        drop(sessions);
        self.sessions
            .write()
            .insert(new_state.session_id.clone(), new_state.clone());
        Some(new_state)
    }

    /// List all active sessions.
    pub fn list_sessions(&self) -> Vec<(String, String)> {
        self.sessions
            .read()
            .values()
            .map(|s| (s.session_id.clone(), s.cwd.clone()))
            .collect()
    }

    /// Cancel a running session.
    pub fn cancel_session(&self, session_id: &str) -> bool {
        if let Some(state) = self.sessions.read().get(session_id) {
            *state.cancelled.write() = true;
            true
        } else {
            false
        }
    }

    /// Check if a session has been cancelled.
    pub fn is_cancelled(&self, session_id: &str) -> bool {
        self.sessions
            .read()
            .get(session_id)
            .map(|s| *s.cancelled.read())
            .unwrap_or(false)
    }

    /// Clear the cancelled flag (for session reuse).
    pub fn clear_cancelled(&self, session_id: &str) {
        if let Some(state) = self.sessions.read().get(session_id) {
            *state.cancelled.write() = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_get() {
        let mgr = SessionManager::new();
        let state = mgr.create_session("/tmp");
        assert!(!state.session_id.is_empty());
        assert_eq!(state.cwd, "/tmp");

        let got = mgr.get_session(&state.session_id).unwrap();
        assert_eq!(got.session_id, state.session_id);
    }

    #[test]
    fn test_remove_session() {
        let mgr = SessionManager::new();
        let state = mgr.create_session("/tmp");
        mgr.remove_session(&state.session_id);
        assert!(mgr.get_session(&state.session_id).is_none());
    }

    #[test]
    fn test_update_cwd() {
        let mgr = SessionManager::new();
        let state = mgr.create_session("/old");
        let updated = mgr.update_cwd(&state.session_id, "/new").unwrap();
        assert_eq!(updated.cwd, "/new");
    }

    #[test]
    fn test_fork_session() {
        let mgr = SessionManager::new();
        let state = mgr.create_session("/tmp");
        let forked = mgr.fork_session(&state.session_id, "/other").unwrap();
        assert_ne!(forked.session_id, state.session_id);
        assert_eq!(forked.cwd, "/other");
        // Both sessions should exist
        assert!(mgr.get_session(&state.session_id).is_some());
        assert!(mgr.get_session(&forked.session_id).is_some());
    }

    #[test]
    fn test_list_sessions() {
        let mgr = SessionManager::new();
        mgr.create_session("/a");
        mgr.create_session("/b");
        let list = mgr.list_sessions();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_cancel_session() {
        let mgr = SessionManager::new();
        let state = mgr.create_session("/tmp");
        assert!(!mgr.is_cancelled(&state.session_id));
        assert!(mgr.cancel_session(&state.session_id));
        assert!(mgr.is_cancelled(&state.session_id));
        assert!(!mgr.cancel_session("nonexistent"));
    }

    #[test]
    fn test_update_cwd_not_found() {
        let mgr = SessionManager::new();
        assert!(mgr.update_cwd("nonexistent", "/tmp").is_none());
    }

    #[test]
    fn test_fork_not_found() {
        let mgr = SessionManager::new();
        assert!(mgr.fork_session("nonexistent", "/tmp").is_none());
    }
}
