#![allow(dead_code)]
//! Debug session infrastructure.
//!
//! Records tool calls to JSON log files when debug mode is activated
//! via environment variable. Mirrors the Python `tools/debug_helpers.py`.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use hermes_core::hermes_home::get_hermes_home;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single debug log entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct DebugCall {
    pub timestamp: String,
    pub call_name: String,
    pub data: serde_json::Value,
}

/// Debug session that records tool calls to JSON log files.
///
/// Activated via environment variable (e.g., `HERMES_DEBUG_TOOLS=true`).
/// Cheap no-op when disabled.
#[derive(Debug)]
pub struct DebugSession {
    pub tool_name: String,
    pub env_var: String,
    pub enabled: bool,
    pub session_id: Option<Uuid>,
    pub calls: Vec<DebugCall>,
    pub log_dir: PathBuf,
}

impl DebugSession {
    /// Create a new debug session.
    ///
    /// Activated if the specified environment variable is set to "true".
    pub fn new(tool_name: &str, env_var: &str) -> Self {
        let enabled = std::env::var(env_var)
            .map(|v| hermes_core::coerce_bool(&v))
            .unwrap_or(false);

        let session_id = if enabled { Some(Uuid::new_v4()) } else { None };

        let log_dir = get_hermes_home().join("logs");

        Self {
            tool_name: tool_name.to_string(),
            env_var: env_var.to_string(),
            enabled,
            session_id,
            calls: Vec::new(),
            log_dir,
        }
    }

    /// Log a tool call. No-op when disabled.
    pub fn log_call(&mut self, call_name: &str, data: serde_json::Value) {
        if !self.enabled {
            return;
        }

        let timestamp = format_timestamp(SystemTime::now());

        self.calls.push(DebugCall {
            timestamp,
            call_name: call_name.to_string(),
            data,
        });
    }

    /// Flush log entries to a JSON file. No-op when disabled.
    pub fn save(&self) -> std::io::Result<()> {
        if !self.enabled || self.session_id.is_none() {
            return Ok(());
        }

        let session_id = self.session_id.unwrap();
        let log_path = self.log_dir.join(format!(
            "{}_debug_{}.json",
            self.tool_name, session_id
        ));

        fs::create_dir_all(&self.log_dir)?;

        let content = serde_json::to_string_pretty(&self.calls)?;
        fs::write(&log_path, content)?;

        Ok(())
    }

    /// Get session info as a JSON Value.
    pub fn get_session_info(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "session_id": self.session_id.map(|u| u.to_string()),
            "log_path": if self.enabled {
                Some(self.log_dir.join(format!(
                    "{}_debug_{}.json",
                    self.tool_name,
                    self.session_id.map(|u| u.to_string()).unwrap_or_default()
                )).display().to_string())
            } else {
                None
            },
            "total_calls": self.calls.len(),
        })
    }
}

/// Format a SystemTime as an ISO 8601 timestamp.
fn format_timestamp(time: SystemTime) -> String {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = time.into();
    dt.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_session() {
        let mut session = DebugSession::new("test_tool", "HERMES_DEBUG_TEST_TOOL");
        assert!(!session.enabled);
        session.log_call("do_thing", serde_json::json!({}));
        assert!(session.calls.is_empty());
    }

    #[test]
    fn test_enabled_session() {
        std::env::set_var("HERMES_DEBUG_TEST_TOOL_ENABLED", "true");
        let mut session = DebugSession::new("test_tool", "HERMES_DEBUG_TEST_TOOL_ENABLED");
        assert!(session.enabled);
        assert!(session.session_id.is_some());

        session.log_call("do_thing", serde_json::json!({"key": "value"}));
        assert_eq!(session.calls.len(), 1);

        let info = session.get_session_info();
        assert_eq!(info["total_calls"], 1);
        std::env::remove_var("HERMES_DEBUG_TEST_TOOL_ENABLED");
    }
}
