#![allow(dead_code)]
//! Authentication subcommands.
//!
//! Mirrors Python: hermes auth add/list/remove/reset, hermes login/logout

use std::path::PathBuf;

use console::Style;
use hermes_llm::credential_pool::{from_entries, AuthType, Credential, CredentialPool, CredentialSource};

/// Add a pooled credential.
pub fn cmd_auth_add(
    provider: &str,
    auth_type: &str,
    key: Option<&str>,
    label: Option<&str>,
    client_id: Option<&str>,
    no_browser: bool,
    _portal_url: Option<&str>,
    _inference_url: Option<&str>,
    _scope: Option<&str>,
    _timeout: Option<f64>,
    _insecure: bool,
    _ca_bundle: Option<&str>,
) -> anyhow::Result<()> {
    let green = Style::new().green();
    let cyan = Style::new().cyan();
    let yellow = Style::new().yellow();

    let cred_path = credential_store_path();
    let mut creds = load_credentials(&cred_path).unwrap_or_default();

    // For OAuth types, we need client_id
    if auth_type == "oauth" && client_id.is_none() {
        println!("  {} OAuth auth type requires --client-id.", yellow.apply_to("⚠"));
        return Ok(());
    }

    // For API key type, require a key
    if auth_type == "api-key" && key.is_none() {
        println!("  {} API key auth type requires --key.", yellow.apply_to("⚠"));
        return Ok(());
    }

    let label_str = label.unwrap_or(provider).to_string();
    let api_key = key.unwrap_or("").to_string();
    creds.push(CredentialEntry {
        provider: provider.to_string(),
        api_key: api_key.clone(),
        label: label_str.clone(),
        exhausted: false,
    });

    save_credentials(&cred_path, &creds)?;

    // Also persist to auth.json credential_pool for parity with Python
    let mut auth_store = load_auth_store()?;
    let pool = read_credential_pool_from_auth(&auth_store, provider)
        .unwrap_or_else(|| CredentialPool::new(provider.to_string(), Vec::new()));
    let auth_type_enum = if auth_type == "oauth" {
        AuthType::OAuth
    } else {
        AuthType::ApiKey
    };
    let credential = Credential {
        id: Credential::new(String::new()).id,
        label: label_str.clone(),
        auth_type: auth_type_enum,
        priority: 0,
        source: CredentialSource::Manual,
        access_token: api_key,
        refresh_token: None,
        expires_at: None,
        expires_at_ms: None,
        last_refresh: None,
        inference_base_url: None,
        base_url: None,
        agent_key: None,
        agent_key_expires_at: None,
        request_count: 0,
        last_status: None,
        last_status_at: None,
        last_error_code: None,
        last_error_reason: None,
        last_error_message: None,
        last_error_reset_at: None,
        extra: std::collections::HashMap::new(),
    };
    pool.add_entry(credential);
    write_credential_pool_to_auth(&mut auth_store, provider, &pool)?;
    save_auth_store(&mut auth_store)?;

    println!();
    println!("{}", cyan.apply_to("◆ Credential Added"));
    println!("  {} Provider: {provider}", green.apply_to("✓"));
    println!("  Type:       {auth_type}");
    println!("  Label:      {label_str}");
    if no_browser {
        println!("  Browser:    disabled");
    }
    println!();

    Ok(())
}

/// List pooled credentials.
pub fn cmd_auth_list(provider_filter: Option<&str>) -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    let cred_path = credential_store_path();
    let creds = load_credentials(&cred_path).unwrap_or_default();
    let auth_store = load_auth_store()?;

    // Collect legacy credentials.json entries
    let mut all_entries: Vec<(String, String, String, bool)> = Vec::new();
    for cred in &creds {
        if provider_filter.is_none_or(|p| cred.provider == p) {
            all_entries.push((
                cred.provider.clone(),
                cred.label.clone(),
                cred.api_key.clone(),
                cred.exhausted,
            ));
        }
    }

    // Collect auth.json credential_pool entries
    if let Some(pool_obj) = auth_store.credential_pool.as_ref().and_then(|v| v.as_object()) {
        for (provider_id, entries_val) in pool_obj {
            if provider_filter.is_none_or(|p| provider_id == p) {
                if let Ok(entries_arr) = serde_json::from_value::<Vec<serde_json::Value>>(entries_val.clone()) {
                    for entry in entries_arr {
                        if let Some(obj) = entry.as_object() {
                            let label = obj.get("label").and_then(|v| v.as_str()).unwrap_or(provider_id).to_string();
                            let api_key = obj.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let exhausted = obj.get("last_status").and_then(|v| v.as_str()) == Some("exhausted");
                            all_entries.push((provider_id.clone(), label, api_key, exhausted));
                        }
                    }
                }
            }
        }
    }

    println!();
    println!("{}", cyan.apply_to("◆ Pooled Credentials"));
    if let Some(p) = provider_filter {
        println!("  Filter: {p}");
    }
    println!();

    if all_entries.is_empty() {
        println!("  {}", dim.apply_to("No credentials configured."));
        if provider_filter.is_some() {
            println!("  Try: hermes auth list (no filter) to see all credentials.");
        } else {
            println!("  Add one with: hermes auth add <provider> --key <api_key>");
        }
        println!();
        return Ok(());
    }

    for (i, (provider, label, api_key, exhausted)) in all_entries.iter().enumerate() {
        let masked = mask_api_key(api_key);
        let status = if *exhausted {
            yellow.apply_to("exhausted").to_string()
        } else {
            green.apply_to("active").to_string()
        };
        println!("  {i:>2}. {}  [{}]  {masked}  {}", provider, label, status);
    }
    println!();
    println!("  Total: {} credential(s)", all_entries.len());
    println!();

    Ok(())
}

/// Remove a credential by provider + target (index, id, or label).
pub fn cmd_auth_remove(provider: &str, target: &str) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let cred_path = credential_store_path();
    let mut creds = load_credentials(&cred_path).unwrap_or_default();
    let mut auth_store = load_auth_store()?;
    let mut removed_from_auth = false;

    // Try to remove from auth.json credential_pool first
    if let Some(pool) = read_credential_pool_from_auth(&auth_store, provider) {
        let before = pool.len();
        let entries: Vec<_> = pool.entries();
        let mut kept = Vec::new();
        for entry in entries {
            let should_remove = if let Ok((_, resolved)) = pool.resolve_target(target) {
                entry.id == resolved.id
            } else {
                entry.label == target || entry.access_token == target
            };
            if !should_remove {
                kept.push(entry);
            }
        }
        if kept.len() < before {
            let new_pool = CredentialPool::new(provider.to_string(), kept);
            write_credential_pool_to_auth(&mut auth_store, provider, &new_pool)?;
            save_auth_store(&mut auth_store)?;
            removed_from_auth = true;
        }
    }

    // Try to parse target as index first
    let mut removed_from_legacy = false;
    if let Ok(index) = target.parse::<usize>() {
        // Filter by provider first, then index within that provider
        let provider_creds: Vec<(usize, &CredentialEntry)> = creds.iter()
            .enumerate()
            .filter(|(_, c)| c.provider == provider)
            .collect();

        if index >= provider_creds.len() {
            if !removed_from_auth {
                println!("  {} Index {index} out of range for provider '{provider}' ({} credentials).", yellow.apply_to("✗"), provider_creds.len());
            }
        } else {
            let (original_idx, removed) = provider_creds[index];
            let removed_provider = removed.provider.clone();
            let removed_label = removed.label.clone();
            creds.remove(original_idx);
            save_credentials(&cred_path, &creds)?;
            println!("  {} Removed credential: {} ({})", green.apply_to("✓"), removed_provider, removed_label);
            removed_from_legacy = true;
        }
    } else {
        // Try to match by label
        let before = creds.len();
        creds.retain(|c| !(c.provider == provider && (c.label == target || c.api_key == target)));
        if creds.len() < before {
            save_credentials(&cred_path, &creds)?;
            println!("  {} Removed credential: {} ({})", green.apply_to("✓"), provider, target);
            removed_from_legacy = true;
        }
    }

    if removed_from_auth && !removed_from_legacy {
        println!("  {} Removed credential from auth.json: {} ({})", green.apply_to("✓"), provider, target);
    }

    if !removed_from_auth && !removed_from_legacy {
        println!("  {} No credential matching target '{}' for provider '{}'.", yellow.apply_to("✗"), target, provider);
    }
    println!();

    Ok(())
}

/// Reset exhaustion status for a provider.
pub fn cmd_auth_reset(provider: &str) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let cred_path = credential_store_path();
    let mut creds = load_credentials(&cred_path).unwrap_or_default();

    let mut reset_count = 0;
    for cred in &mut creds {
        if cred.provider == provider && cred.exhausted {
            cred.exhausted = false;
            reset_count += 1;
        }
    }

    if reset_count > 0 {
        save_credentials(&cred_path, &creds)?;
    }

    // Also reset auth.json credential_pool entries
    let mut auth_store = load_auth_store()?;
    let mut reset_auth_count = 0;
    if let Some(pool) = read_credential_pool_from_auth(&auth_store, provider) {
        let mut entries = pool.entries();
        let mut modified = false;
        for entry in &mut entries {
            if entry.last_status.is_some() {
                entry.last_status = None;
                entry.last_status_at = None;
                entry.last_error_code = None;
                entry.last_error_reason = None;
                entry.last_error_message = None;
                entry.last_error_reset_at = None;
                modified = true;
                reset_auth_count += 1;
            }
        }
        if modified {
            let new_pool = CredentialPool::new(provider.to_string(), entries);
            write_credential_pool_to_auth(&mut auth_store, provider, &new_pool)?;
            save_auth_store(&mut auth_store)?;
        }
    }

    let total_reset = reset_count + reset_auth_count;
    if total_reset > 0 {
        println!("  {} Reset {total_reset} credential(s) for '{provider}'.", green.apply_to("✓"));
    } else {
        println!("  {} No exhausted credentials found for '{provider}'.", yellow.apply_to("→"));
    }
    println!();

    Ok(())
}

/// Logout — clear stored credentials.
pub fn cmd_logout(provider: Option<&str>) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let cred_path = credential_store_path();
    let mut auth_store = load_auth_store()?;

    match provider {
        Some(p) => {
            let mut creds = load_credentials(&cred_path).unwrap_or_default();
            let before = creds.len();
            creds.retain(|c| c.provider != p);
            let removed_legacy = before - creds.len();
            if removed_legacy > 0 {
                save_credentials(&cred_path, &creds)?;
            }

            // Clear auth.json provider state and credential_pool for this provider
            let mut removed_auth = 0;
            if auth_store.providers.remove(p).is_some() {
                removed_auth += 1;
            }
            if let Some(pool_val) = auth_store.credential_pool.as_mut().and_then(|v| v.as_object_mut()) {
                if pool_val.remove(p).is_some() {
                    removed_auth += 1;
                }
            }
            if auth_store.active_provider.as_deref() == Some(p) {
                auth_store.active_provider = None;
                removed_auth += 1;
            }
            if removed_auth > 0 {
                save_auth_store(&mut auth_store)?;
            }

            let total_removed = removed_legacy + if removed_auth > 0 { 1 } else { 0 };
            if total_removed > 0 {
                println!("  {} Logged out from '{p}'.", green.apply_to("✓"));
            } else {
                println!("  {} No credentials found for '{p}'.", yellow.apply_to("→"));
            }
        }
        None => {
            let mut removed = false;
            if cred_path.exists() {
                std::fs::remove_file(&cred_path)?;
                removed = true;
            }

            let auth_path = auth_store_path();
            if auth_path.exists() {
                auth_store.providers.clear();
                auth_store.credential_pool = None;
                auth_store.active_provider = None;
                auth_store.suppressed_sources = None;
                save_auth_store(&mut auth_store)?;
                removed = true;
            }

            if removed {
                println!("  {} Logged out — all credentials cleared.", green.apply_to("✓"));
            } else {
                println!("  {} No credentials found.", yellow.apply_to("→"));
            }
        }
    }
    println!();

    Ok(())
}

/// Show auth status.
pub fn cmd_auth_status() -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let cred_path = credential_store_path();
    let creds = load_credentials(&cred_path).unwrap_or_default();
    let auth_store = load_auth_store()?;

    println!();
    println!("{}", cyan.apply_to("◆ Auth Status"));
    println!();

    if creds.is_empty() && auth_store.providers.is_empty() {
        println!("  {}", yellow.apply_to("No credentials configured."));
    } else {
        let active = creds.iter().filter(|c| !c.exhausted).count();
        let exhausted = creds.iter().filter(|c| c.exhausted).count();
        println!("  {} {} active, {} exhausted", green.apply_to("✓"), active, exhausted);

        if let Some(ref active_provider) = auth_store.active_provider {
            println!("  {} Active provider: {active_provider}", green.apply_to("✓"));
        }

        for (provider_id, state) in &auth_store.providers {
            let has_token = state.access_token.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
                || state.tokens.as_ref().and_then(|t| t.access_token.as_ref()).map(|s| !s.is_empty()).unwrap_or(false);
            if has_token {
                println!("    {} {provider_id} (auth.json)", green.apply_to("✓"));
            }
        }
    }

    // Check env-based auth
    let env_providers = [
        ("OPENAI", "OPENAI_API_KEY"),
        ("ANTHROPIC", "ANTHROPIC_API_KEY"),
        ("OPENROUTER", "OPENROUTER_API_KEY"),
        ("GOOGLE", "GOOGLE_API_KEY"),
        ("NOUS", "NOUS_API_KEY"),
    ];

    println!();
    println!("  {}", cyan.apply_to("Environment Credentials:"));
    for (name, env_var) in &env_providers {
        if std::env::var(env_var).is_ok() {
            println!("    {} {name} ({env_var} set)", green.apply_to("✓"));
        }
    }
    println!();

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CredentialEntry {
    provider: String,
    api_key: String,
    label: String,
    exhausted: bool,
}

fn credential_store_path() -> PathBuf {
    let hermes_home = hermes_core::get_hermes_home();
    hermes_home.join("credentials.json")
}

fn load_credentials(path: &PathBuf) -> Option<Vec<CredentialEntry>> {
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_credentials(path: &PathBuf, creds: &[CredentialEntry]) -> anyhow::Result<()> {
    let data = serde_json::to_string_pretty(creds)?;
    std::fs::write(path, data)?;
    Ok(())
}

fn mask_api_key(key: &str) -> String {
    if key.len() <= 8 {
        "****".to_string()
    } else {
        format!("{}****{}", &key[..4], &key[key.len() - 4..])
    }
}

const AUTH_STORE_VERSION: u32 = 1;

fn auth_store_path() -> PathBuf {
    let hermes_home = hermes_core::get_hermes_home();
    hermes_home.join("auth.json")
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct AuthStore {
    #[serde(default)]
    pub(crate) version: u32,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub(crate) providers: std::collections::HashMap<String, ProviderState>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) active_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) credential_pool: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) suppressed_sources: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProviderState {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) tokens: Option<CodexTokens>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct CodexTokens {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) access_token: Option<String>,
}

pub(crate) fn load_auth_store() -> anyhow::Result<AuthStore> {
    hermes_core::with_auth_json_read_lock(|| {
        let path = auth_store_path();
        if !path.exists() {
            return AuthStore::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => AuthStore::default(),
        }
    }).map_err(|e| anyhow::anyhow!("Failed to read auth.json: {e}"))
}

pub(crate) fn save_auth_store(auth_store: &mut AuthStore) -> anyhow::Result<PathBuf> {
    hermes_core::with_auth_json_write_lock(|| {
        let path = auth_store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        auth_store.version = AUTH_STORE_VERSION;
        auth_store.updated_at = Some(chrono::Utc::now().to_rfc3339());
        let payload = serde_json::to_string_pretty(auth_store)? + "\n";
        let tmp_name = format!(
            "auth.json.tmp.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let tmp_path = path.with_file_name(&tmp_name);
        std::fs::write(&tmp_path, payload)?;
        std::fs::rename(&tmp_path, &path)?;
        Ok(path)
    })
    .map_err(|e| anyhow::anyhow!("Failed to lock auth.json for writing: {e}"))?
}

fn read_credential_pool_from_auth(auth_store: &AuthStore, provider_id: &str) -> Option<CredentialPool> {
    let pool = auth_store.credential_pool.as_ref()?;
    let pools = pool.as_object()?;
    let entries = pools.get(provider_id)?;
    let entries_arr: Vec<serde_json::Value> = serde_json::from_value(entries.clone()).ok()?;
    from_entries(provider_id, entries_arr)
}

fn write_credential_pool_to_auth(
    auth_store: &mut AuthStore,
    provider_id: &str,
    pool: &CredentialPool,
) -> anyhow::Result<()> {
    let entries = pool.to_json()?;
    let pool_val = auth_store
        .credential_pool
        .get_or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = pool_val.as_object_mut() {
        obj.insert(provider_id.to_string(), entries);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_api_key() {
        assert_eq!(mask_api_key("sk-abc"), "****");
        assert_eq!(mask_api_key("sk-1234567890abcdef"), "sk-1****cdef");
    }

    #[test]
    fn test_credential_store_path() {
        let path = credential_store_path();
        assert!(path.to_string_lossy().contains("credentials.json"));
    }

    #[test]
    fn test_load_nonexistent() {
        assert!(load_credentials(&PathBuf::from("/nonexistent")).is_none());
    }
}
