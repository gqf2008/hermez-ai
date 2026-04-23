//! Browser session manager.
//!
//! Tracks active browser sessions per task_id, handles session creation
//! on first access, and provides cleanup hooks.
//! Mirrors the Python `_active_sessions` + `_get_session_info` pattern.
//!
//! Enhanced with:
//! - Background inactivity-cleanup thread (default 300s)
//! - Orphan reaper for stale agent-browser socket dirs
//! - Cross-process-safe owner_pid tracking

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use parking_lot::Mutex as ParkingLotMutex;

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

/// Inactivity timeout in seconds — default 5 minutes.
fn inactivity_timeout_secs() -> u64 {
    std::env::var("BROWSER_INACTIVITY_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

/// Thread-safe browser session tracker with background cleanup.
pub struct BrowserSessionManager {
    sessions: Arc<ParkingLotMutex<HashMap<String, BrowserSessionInfo>>>,
    cleanup_running: Arc<AtomicBool>,
    cleanup_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl BrowserSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(ParkingLotMutex::new(HashMap::new())),
            cleanup_running: Arc::new(AtomicBool::new(false)),
            cleanup_handle: Mutex::new(None),
        }
    }

    /// Start the background cleanup thread if not already running.
    fn start_cleanup_thread(&self) {
        let mut handle = self.cleanup_handle.lock().unwrap();
        if handle.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }

        self.cleanup_running.store(true, Ordering::SeqCst);
        let sessions = Arc::clone(&self.sessions);
        let running = Arc::clone(&self.cleanup_running);

        let h = std::thread::spawn(move || {
            // One-time orphan reap on startup
            if let Err(e) = reap_orphaned_browser_sessions() {
                tracing::warn!("Orphan reap error: {}", e);
            }

            while running.load(Ordering::SeqCst) {
                cleanup_inactive_browser_sessions(&sessions, inactivity_timeout_secs());
                // Sleep in 1-second intervals so we stop quickly
                for _ in 0..30 {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        });

        *handle = Some(h);
        tracing::info!(
            "Started browser inactivity cleanup thread (timeout: {}s)",
            inactivity_timeout_secs()
        );
    }

    /// Get or create a session for a task_id.
    /// If the session exists, update last_accessed and return it.
    pub fn get_session(&self, task_id: &str) -> Option<BrowserSessionInfo> {
        self.start_cleanup_thread();
        let mut sessions = self.sessions.lock();
        let info = sessions.get_mut(task_id)?;
        info.last_accessed = Instant::now();
        Some(info.clone())
    }

    /// Register a new cloud browser session.
    pub fn register_cloud(&self, task_id: &str, session: &CloudSession) -> BrowserSessionInfo {
        self.start_cleanup_thread();
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
        write_owner_pid(&info.session_name);
        info
    }

    /// Register a Camofox session.
    pub fn register_camofox(&self, task_id: &str, user_id: &str, tab_id: &str) -> BrowserSessionInfo {
        self.start_cleanup_thread();
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
        self.start_cleanup_thread();
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
        write_owner_pid(&info.session_name);
        info
    }

    /// Update the last activity timestamp for a session.
    pub fn update_activity(&self, task_id: &str) {
        let mut sessions = self.sessions.lock();
        if let Some(info) = sessions.get_mut(task_id) {
            info.last_accessed = Instant::now();
        }
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
    pub fn idle_sessions(&self, timeout: Duration) -> Vec<BrowserSessionInfo> {
        let now = Instant::now();
        self.sessions
            .lock()
            .values()
            .filter(|s| now.duration_since(s.last_accessed) > timeout)
            .cloned()
            .collect()
    }

    /// Stop the background cleanup thread.
    pub fn stop_cleanup(&self) {
        self.cleanup_running.store(false, Ordering::SeqCst);
        if let Some(h) = self.cleanup_handle.lock().unwrap().take() {
            let _ = h.join();
        }
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

// ============================================================================
// Inactivity cleanup
// ============================================================================

fn cleanup_inactive_browser_sessions(
    sessions: &Arc<ParkingLotMutex<HashMap<String, BrowserSessionInfo>>>,
    timeout_secs: u64,
) {
    let now = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let to_clean: Vec<String> = {
        let lock = sessions.lock();
        lock.values()
            .filter(|s| now.duration_since(s.last_accessed) > timeout)
            .map(|s| s.task_id.clone())
            .collect()
    };

    for task_id in to_clean {
        let elapsed = {
            let lock = sessions.lock();
            if let Some(info) = lock.get(&task_id) {
                now.duration_since(info.last_accessed).as_secs()
            } else {
                continue;
            }
        };
        tracing::info!(
            "Cleaning up inactive browser session for task: {} (inactive for {}s)",
            task_id,
            elapsed
        );
        // Best-effort: remove from tracking.  Full cleanup (kill daemon, close
        // cloud session) is done by the caller if desired.
        sessions.lock().remove(&task_id);
    }
}

// ============================================================================
// Orphan reaper
// ============================================================================

/// Return the temp directory suitable for Unix domain sockets.
/// On macOS we bypass the long `TMPDIR` and use `/tmp` directly.
fn socket_safe_tmpdir() -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    }
}

/// Write the current process PID as the owner of a browser socket dir.
fn write_owner_pid(session_name: &str) {
    let socket_dir = socket_safe_tmpdir().join(format!("agent-browser-{session_name}"));
    let path = socket_dir.join(format!("{session_name}.owner_pid"));
    if let Err(e) = std::fs::create_dir_all(&socket_dir) {
        tracing::debug!("Could not create socket dir for owner_pid: {}", e);
        return;
    }
    let pid = std::process::id();
    if let Err(e) = std::fs::write(&path, pid.to_string()) {
        tracing::debug!("Could not write owner_pid file for {}: {}", session_name, e);
    }
}

/// Scan for orphaned agent-browser daemon processes from previous runs.
///
/// Mirrors Python `_reap_orphaned_browser_sessions`.
fn reap_orphaned_browser_sessions() -> Result<(), String> {
    let tmpdir = socket_safe_tmpdir();
    let mut socket_dirs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&tmpdir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("agent-browser-h_") || name_str.starts_with("agent-browser-cdp_") {
                socket_dirs.push(entry.path());
            }
        }
    }

    if socket_dirs.is_empty() {
        return Ok(());
    }

    let mut reaped = 0usize;
    for socket_dir in socket_dirs {
        let dir_name = socket_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let session_name = dir_name.strip_prefix("agent-browser-").unwrap_or("");
        if session_name.is_empty() {
            continue;
        }

        // Ownership check 1: owner_pid file (cross-process safe)
        let owner_pid_file = socket_dir.join(format!("{session_name}.owner_pid"));
        let mut owner_alive: Option<bool> = None;
        if owner_pid_file.is_file() {
            if let Ok(text) = std::fs::read_to_string(&owner_pid_file) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    #[cfg(unix)]
                    {
                        unsafe {
                            let r = libc::kill(pid as libc::pid_t, 0);
                            if r == 0 {
                                owner_alive = Some(true);
                            } else {
                                let err = std::io::Error::last_os_error();
                                if err.raw_os_error() == Some(libc::ESRCH) {
                                    owner_alive = Some(false);
                                } else {
                                    // Permission denied or other → treat as alive
                                    owner_alive = Some(true);
                                }
                            }
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        // On non-Unix, fall through to legacy heuristic
                        owner_alive = None;
                    }
                }
            }
        }

        if owner_alive == Some(true) {
            continue;
        }

        // No owner_pid or dead owner → reap the daemon
        let pid_file = socket_dir.join(format!("{session_name}.pid"));
        if !pid_file.is_file() {
            let _ = std::fs::remove_dir_all(&socket_dir);
            continue;
        }

        let daemon_pid = match std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            Some(p) => p,
            None => {
                let _ = std::fs::remove_dir_all(&socket_dir);
                continue;
            }
        };

        #[cfg(unix)]
        {
            unsafe {
                let r = libc::kill(daemon_pid as libc::pid_t, 0);
                if r != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ESRCH) {
                        // Already dead
                        let _ = std::fs::remove_dir_all(&socket_dir);
                        continue;
                    } else if err.raw_os_error() == Some(libc::EPERM) {
                        // Alive but owned by someone else → leave it
                        continue;
                    }
                }
                // Send SIGTERM
                libc::kill(daemon_pid as libc::pid_t, libc::SIGTERM);
                reaped += 1;
                tracing::info!(
                    "Reaped orphaned browser daemon PID {} (session {})",
                    daemon_pid,
                    session_name
                );
            }
        }

        let _ = std::fs::remove_dir_all(&socket_dir);
    }

    if reaped > 0 {
        tracing::info!("Reaped {} orphaned browser session(s)", reaped);
    }
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_get_cloud_session() {
        let mgr = BrowserSessionManager::new();
        let session = CloudSession {
            session_name: "hermez-test".to_string(),
            provider_session_id: "bb-123".to_string(),
            cdp_url: Some("ws://localhost:9222".to_string()),
            features: Default::default(),
        };
        mgr.register_cloud("task-1", &session);

        let info = mgr.get_session("task-1").unwrap();
        assert_eq!(info.session_name, "hermez-test");
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
        mgr.register_local("task-1", "hermez-task-1");

        let info = mgr.get_session("task-1").unwrap();
        assert_eq!(info.session_name, "hermez-task-1");
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
        mgr.register_local("task-1", "hermez-task-1");
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
