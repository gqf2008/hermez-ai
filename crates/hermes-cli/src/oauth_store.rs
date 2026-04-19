#![allow(dead_code)]
//! Secure credential storage with OS keychain fallback.
//!
//! Mirrors Python `auth.py` secure storage patterns:
//! - Primary: OS keyring (macOS Keychain, Windows Credential Manager, Linux Secret Service)
//! - Fallback: plaintext JSON file in ~/.hermes/ (protected by 0o600 permissions on Unix)
//!
//! Service names are scoped as `hermes-agent/{provider}` to avoid collisions.

use std::path::PathBuf;

/// Errors from the secure store.
#[derive(Debug, Clone, PartialEq)]
pub enum SecureStoreError {
    /// OS keyring is unavailable or the entry was not found.
    NotFound,
    /// The OS keyring returned an error.
    KeyringError(String),
    /// File I/O error on the fallback path.
    IoError(String),
    /// Serialization error.
    JsonError(String),
}

impl std::fmt::Display for SecureStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "Credential not found in secure store"),
            Self::KeyringError(msg) => write!(f, "Keyring error: {msg}"),
            Self::IoError(msg) => write!(f, "I/O error: {msg}"),
            Self::JsonError(msg) => write!(f, "JSON error: {msg}"),
        }
    }
}

impl std::error::Error for SecureStoreError {}

/// A stored OAuth credential.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredCredential {
    pub provider: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
}

/// Store a credential securely.
///
/// Tries the OS keyring first; on failure falls back to a JSON file
/// in `~/.hermes/secure_credentials/{provider}.json`.
pub fn store_credential(cred: &StoredCredential) -> Result<(), SecureStoreError> {
    let service = service_name(&cred.provider);
    let account = "default";
    let payload = serde_json::to_string(cred)
        .map_err(|e| SecureStoreError::JsonError(e.to_string()))?;

    // Try OS keyring first
    match keyring::Entry::new(&service, account) {
        Ok(entry) => match entry.set_password(&payload) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    "Keyring store failed for {} (falling back to file): {}",
                    cred.provider,
                    e
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                "Keyring entry creation failed for {} (falling back to file): {}",
                cred.provider,
                e
            );
        }
    }

    // Fallback: encrypted file storage
    let path = fallback_path(&cred.provider);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SecureStoreError::IoError(e.to_string()))?;
    }
    std::fs::write(&path, payload)
        .map_err(|e| SecureStoreError::IoError(e.to_string()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .map_err(|e| SecureStoreError::IoError(e.to_string()))?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)
            .map_err(|e| SecureStoreError::IoError(e.to_string()))?;
    }

    Ok(())
}

/// Retrieve a credential by provider.
///
/// Checks the OS keyring first, then falls back to the file store.
pub fn retrieve_credential(provider: &str) -> Result<StoredCredential, SecureStoreError> {
    let service = service_name(provider);
    let account = "default";

    // Try OS keyring first
    if let Ok(entry) = keyring::Entry::new(&service, account) {
        match entry.get_password() {
            Ok(payload) => {
                return serde_json::from_str(&payload)
                    .map_err(|e| SecureStoreError::JsonError(e.to_string()));
            }
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                tracing::warn!("Keyring retrieve failed for {provider}: {e}");
            }
        }
    }

    // Fallback: file store
    let path = fallback_path(provider);
    if !path.exists() {
        return Err(SecureStoreError::NotFound);
    }
    let payload = std::fs::read_to_string(&path)
        .map_err(|e| SecureStoreError::IoError(e.to_string()))?;
    serde_json::from_str(&payload)
        .map_err(|e| SecureStoreError::JsonError(e.to_string()))
}

/// Delete a stored credential.
pub fn delete_credential(provider: &str) -> Result<(), SecureStoreError> {
    let service = service_name(provider);
    let account = "default";

    let mut deleted = false;

    if let Ok(entry) = keyring::Entry::new(&service, account) {
        match entry.delete_credential() {
            Ok(()) => deleted = true,
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                tracing::warn!("Keyring delete failed for {provider}: {e}");
            }
        }
    }

    let path = fallback_path(provider);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            if !deleted {
                return Err(SecureStoreError::IoError(e.to_string()));
            }
        } else {
            deleted = true;
        }
    }

    if deleted {
        Ok(())
    } else {
        Err(SecureStoreError::NotFound)
    }
}

/// List providers that have stored credentials.
pub fn list_stored_providers() -> Vec<String> {
    let mut providers = Vec::new();

    // Check fallback directory
    let dir = hermes_core::get_hermes_home().join("secure_credentials");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(s) = name.to_str() {
                if s.ends_with(".json") {
                    providers.push(s.trim_end_matches(".json").to_string());
                }
            }
        }
    }

    providers.sort();
    providers.dedup();
    providers
}

fn service_name(provider: &str) -> String {
    format!("hermes-agent/{provider}")
}

fn fallback_path(provider: &str) -> PathBuf {
    hermes_core::get_hermes_home()
        .join("secure_credentials")
        .join(format!("{provider}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_name() {
        assert_eq!(service_name("google"), "hermes-agent/google");
    }

    #[test]
    fn test_fallback_path_contains_provider() {
        let path = fallback_path("github");
        let s = path.to_string_lossy();
        assert!(s.contains("secure_credentials"));
        assert!(s.contains("github.json"));
    }

    #[test]
    fn test_delete_nonexistent() {
        // Should fail gracefully when nothing is stored
        let result = delete_credential("test-nonexistent-provider-xyz");
        assert!(matches!(result, Err(SecureStoreError::NotFound)));
    }
}
