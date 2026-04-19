//! Hermes-managed Camofox state helpers.
//!
//! Provides profile-scoped identity and state directory paths for Camofox
//! persistent browser profiles.  When managed persistence is enabled, Hermes
//! sends a deterministic userId derived from the active profile so that
//! Camofox can map it to the same persistent browser profile directory
//! across restarts.

use std::path::PathBuf;

const CAMOFOX_STATE_DIR_NAME: &str = "browser_auth";
const CAMOFOX_STATE_SUBDIR: &str = "camofox";

/// Return the profile-scoped root directory for Camofox persistence.
/// Mirrors Python `get_camofox_state_dir()`.
pub fn get_camofox_state_dir() -> PathBuf {
    hermes_core::get_hermes_home()
        .join(CAMOFOX_STATE_DIR_NAME)
        .join(CAMOFOX_STATE_SUBDIR)
}

/// Return the stable Hermes-managed Camofox identity for this profile.
///
/// The user identity is profile-scoped (same Hermes profile = same userId).
/// The session key is scoped to the logical browser task so newly created
/// tabs within the same profile reuse the same identity contract.
///
/// Mirrors Python `get_camofox_identity()`.
pub fn get_camofox_identity(task_id: Option<&str>) -> serde_json::Value {
    let scope_root = get_camofox_state_dir().to_string_lossy().to_string();
    let logical_scope = task_id.unwrap_or("default");

    let user_id = derive_user_id(Some(&scope_root));
    let session_key = derive_session_key(logical_scope, Some(&scope_root));

    serde_json::json!({
        "user_id": user_id,
        "session_key": session_key,
    })
}

/// Derive a deterministic user_id from a profile path (state directory).
///
/// Profile-scoped: same Hermes profile always yields the same user_id,
/// regardless of task_id. This ensures Camofox maps to the same persistent
/// browser profile directory across restarts.
///
/// Mirrors Python `uuid.uuid5(NAMESPACE_URL, f"camofox-user:{scope_root}").hex[:10]`.
pub fn derive_user_id(profile_path: Option<&str>) -> String {
    use uuid::Uuid;

    let scope = profile_path.unwrap_or("default");
    let input = format!("camofox-user:{scope}");
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, input.as_bytes());
    let hex = uuid.as_simple().to_string();
    format!("hermes_{}", &hex[..10])
}

/// Derive a session key from task_id and profile path.
/// Mirrors Python `uuid.uuid5(NAMESPACE_URL, f"camofox-session:{scope_root}:{logical_scope}").hex[:16]`.
pub fn derive_session_key(task_id: &str, profile_path: Option<&str>) -> String {
    use uuid::Uuid;

    let scope = profile_path.unwrap_or("default");
    let input = format!("camofox-session:{scope}:{task_id}");
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, input.as_bytes());
    let hex = uuid.as_simple().to_string();
    format!("task_{}", &hex[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_user_id_deterministic() {
        let id1 = derive_user_id(None);
        let id2 = derive_user_id(None);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("hermes_"));
    }

    #[test]
    fn test_derive_session_key_deterministic() {
        let key1 = derive_session_key("task-1", None);
        let key2 = derive_session_key("task-1", None);
        assert_eq!(key1, key2);
        assert!(key1.starts_with("task_"));
    }

    #[test]
    fn test_derive_user_id_profile_scoped() {
        let id1 = derive_user_id(Some("/tmp/test-profile"));
        let id2 = derive_user_id(Some("/tmp/test-profile"));
        assert_eq!(id1, id2, "same profile should yield same user_id");

        let id3 = derive_user_id(Some("/tmp/other-profile"));
        assert_ne!(id1, id3, "different profiles should yield different user_ids");
    }

    #[test]
    fn test_derive_session_key_task_scoped() {
        let key1 = derive_session_key("task-a", None);
        let key2 = derive_session_key("task-b", None);
        assert_ne!(key1, key2, "different tasks should yield different session_keys");
    }

    #[test]
    fn test_get_camofox_state_dir() {
        let dir = get_camofox_state_dir();
        let path = dir.to_string_lossy();
        assert!(path.contains("browser_auth"));
        assert!(path.contains("camofox"));
    }

    #[test]
    fn test_get_camofox_identity() {
        let identity = get_camofox_identity(Some("my-task"));
        let user_id = identity.get("user_id").and_then(|v| v.as_str()).unwrap();
        let session_key = identity.get("session_key").and_then(|v| v.as_str()).unwrap();

        assert!(user_id.starts_with("hermes_"));
        assert!(session_key.starts_with("task_"));

        let identity2 = get_camofox_identity(Some("my-task"));
        assert_eq!(identity, identity2);
    }
}
