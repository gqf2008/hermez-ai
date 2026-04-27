//! WhatsApp identity canonicalization helpers.
//!
//! WhatsApp's bridge can surface the same human under different JID shapes:
//! - LID form: `999999999999999@lid`
//! - Phone form: `15551234567@s.whatsapp.net`
//!
//! This module collapses these aliases to a single stable identity so that
//! session keys are consistent across JID/LID/phone variants.
//!
//! Mirrors the Python `gateway/whatsapp_identity.py`.

use std::collections::HashMap;

/// Strip WhatsApp JID/LID syntax down to its bare numeric identifier.
///
/// Accepts: `"60123456789@s.whatsapp.net"`, `"60123456789:47@s.whatsapp.net"`,
/// `"60123456789@lid"`, `"+60123456789"`, or bare `"60123456789"`.
/// Returns just the numeric portion for equality comparisons.
///
/// Mirrors Python `normalize_whatsapp_identifier()`.
pub fn normalize_whatsapp_identifier(value: &str) -> String {
    let stripped = value.trim().trim_start_matches('+');
    // Split on ':' first (device suffix), then '@' (domain suffix)
    stripped
        .split(':')
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Return a canonical WhatsApp identifier, resolving LID↔phone aliases.
///
/// Walks `lid-mapping-*.json` files in the Hermez home directory. If a
/// mapping is found that resolves any form of the identifier, returns the
/// canonical form. Otherwise, returns the normalized form.
///
/// Mirrors Python `canonical_whatsapp_identifier()`.
pub fn canonical_whatsapp_identifier(value: &str) -> String {
    let normalized = normalize_whatsapp_identifier(value);
    // Try to resolve via lid-mapping files
    if let Some(canonical) = resolve_lid_alias(&normalized) {
        return canonical;
    }
    normalized
}

/// Lazily-loaded lid-mapping cache (loaded once, static for the process lifetime).
/// Lid mappings are stable files — they don't change between messages.
static LID_MAPPING: std::sync::LazyLock<Option<HashMap<String, String>>> =
    std::sync::LazyLock::new(load_lid_mappings);

/// Load all lid-mapping files from ~/.hermez into a HashMap.
fn load_lid_mappings() -> Option<HashMap<String, String>> {
    let home = hermez_core::get_hermez_home();
    let dir = std::fs::read_dir(home.join("whatsapp").join("session")).ok()?;
    let mut mapping = HashMap::new();

    for entry in dir.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("lid-mapping-") || !name_str.ends_with(".json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(pairs) = parsed.as_array() {
                    for pair in pairs {
                        if let Some(arr) = pair.as_array() {
                            if arr.len() >= 2 {
                                let lid_n = normalize_whatsapp_identifier(arr[0].as_str().unwrap_or(""));
                                let phone_n = normalize_whatsapp_identifier(arr[1].as_str().unwrap_or(""));
                                mapping.insert(lid_n, phone_n.clone());
                                mapping.insert(phone_n.clone(), phone_n);
                            }
                        }
                    }
                }
            }
        }
    }
    Some(mapping)
}

/// Look up a normalized numeric ID in the cached lid-mapping.
fn resolve_lid_alias(normalized: &str) -> Option<String> {
    LID_MAPPING.as_ref()?.get(normalized).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_jid_form() {
        assert_eq!(
            normalize_whatsapp_identifier("60123456789@s.whatsapp.net"),
            "60123456789"
        );
    }

    #[test]
    fn test_normalize_with_device() {
        assert_eq!(
            normalize_whatsapp_identifier("60123456789:47@s.whatsapp.net"),
            "60123456789"
        );
    }

    #[test]
    fn test_normalize_lid_form() {
        assert_eq!(
            normalize_whatsapp_identifier("60123456789@lid"),
            "60123456789"
        );
    }

    #[test]
    fn test_normalize_with_plus() {
        assert_eq!(normalize_whatsapp_identifier("+60123456789"), "60123456789");
    }

    #[test]
    fn test_normalize_bare() {
        assert_eq!(normalize_whatsapp_identifier("60123456789"), "60123456789");
    }

    #[test]
    fn test_normalize_empty() {
        assert_eq!(normalize_whatsapp_identifier(""), "");
    }

    #[test]
    fn test_canonical_without_mapping_returns_normalized() {
        let result = canonical_whatsapp_identifier("60123456789@s.whatsapp.net");
        assert_eq!(result, "60123456789");
    }
}
