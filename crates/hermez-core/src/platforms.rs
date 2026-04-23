#![allow(dead_code)]
//! Shared platform registry for Hermez Agent.
//!
//! Mirrors Python `hermez_cli/platforms.py`.
//! Single source of truth for platform metadata consumed by both
//! skills_config (label display) and tools_config (default toolset
//! resolution).

use std::collections::HashMap;

/// Metadata for a single platform entry.
#[derive(Debug, Clone)]
pub struct PlatformInfo {
    /// Display label with emoji (e.g. "📱 Telegram").
    pub label: &'static str,
    /// Default toolset name for this platform (e.g. "hermez-telegram").
    pub default_toolset: &'static str,
}

/// Ordered platform registry so that TUI menus are deterministic.
///
/// Maps platform key → metadata.
pub static PLATFORMS: &[(&str, PlatformInfo)] = &[
    ("cli",            PlatformInfo { label: "🖥️  CLI",            default_toolset: "hermez-cli" }),
    ("telegram",       PlatformInfo { label: "📱 Telegram",        default_toolset: "hermez-telegram" }),
    ("discord",        PlatformInfo { label: "💬 Discord",         default_toolset: "hermez-discord" }),
    ("slack",          PlatformInfo { label: "💼 Slack",           default_toolset: "hermez-slack" }),
    ("whatsapp",       PlatformInfo { label: "📱 WhatsApp",        default_toolset: "hermez-whatsapp" }),
    ("signal",         PlatformInfo { label: "📡 Signal",          default_toolset: "hermez-signal" }),
    ("bluebubbles",    PlatformInfo { label: "💙 BlueBubbles",     default_toolset: "hermez-bluebubbles" }),
    ("email",          PlatformInfo { label: "📧 Email",           default_toolset: "hermez-email" }),
    ("homeassistant",  PlatformInfo { label: "🏠 Home Assistant",  default_toolset: "hermez-homeassistant" }),
    ("mattermost",     PlatformInfo { label: "💬 Mattermost",      default_toolset: "hermez-mattermost" }),
    ("matrix",         PlatformInfo { label: "💬 Matrix",          default_toolset: "hermez-matrix" }),
    ("dingtalk",       PlatformInfo { label: "💬 DingTalk",        default_toolset: "hermez-dingtalk" }),
    ("feishu",         PlatformInfo { label: "🪽 Feishu",          default_toolset: "hermez-feishu" }),
    ("wecom",          PlatformInfo { label: "💬 WeCom",           default_toolset: "hermez-wecom" }),
    ("wecom_callback", PlatformInfo { label: "💬 WeCom Callback",  default_toolset: "hermez-wecom-callback" }),
    ("weixin",         PlatformInfo { label: "💬 Weixin",          default_toolset: "hermez-weixin" }),
    ("qqbot",          PlatformInfo { label: "💬 QQBot",           default_toolset: "hermez-qqbot" }),
    ("webhook",        PlatformInfo { label: "🔗 Webhook",         default_toolset: "hermez-webhook" }),
    ("api_server",     PlatformInfo { label: "🌐 API Server",      default_toolset: "hermez-api-server" }),
];

/// Build a static HashMap for O(1) lookups.
///
/// Use this when you need repeated lookups; for one-off lookups use
/// `platform_info()` which does a linear scan over the small array.
fn platforms_map() -> &'static HashMap<&'static str, &'static PlatformInfo> {
    use std::sync::OnceLock;
    static MAP: OnceLock<HashMap<&str, &PlatformInfo>> = OnceLock::new();
    MAP.get_or_init(|| {
        PLATFORMS.iter().map(|(k, v)| (*k, v)).collect()
    })
}

/// Return the display label for a platform key, or *default* if unknown.
///
/// Mirrors Python `platform_label()`.
pub fn platform_label<'a>(key: &'a str, default: &'a str) -> &'a str {
    platforms_map()
        .get(key)
        .map(|info| info.label)
        .unwrap_or(default)
}

/// Return the default toolset for a platform key, or *default* if unknown.
///
/// Extension of Python `platform_label()` for toolset resolution.
pub fn platform_toolset<'a>(key: &'a str, default: &'a str) -> &'a str {
    platforms_map()
        .get(key)
        .map(|info| info.default_toolset)
        .unwrap_or(default)
}

/// Return the full `PlatformInfo` for a platform key, if known.
pub fn platform_info(key: &str) -> Option<&'static PlatformInfo> {
    platforms_map().get(key).copied()
}

/// Return `true` if the platform key is known.
pub fn is_known_platform(key: &str) -> bool {
    platforms_map().contains_key(key)
}

/// Return an iterator over all known platform keys.
pub fn platform_keys() -> impl Iterator<Item = &'static str> {
    PLATFORMS.iter().map(|(k, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_label_known() {
        assert_eq!(platform_label("cli", ""), "🖥️  CLI");
        assert_eq!(platform_label("telegram", ""), "📱 Telegram");
        assert_eq!(platform_label("discord", ""), "💬 Discord");
    }

    #[test]
    fn test_platform_label_unknown() {
        assert_eq!(platform_label("unknown_platform", "fallback"), "fallback");
    }

    #[test]
    fn test_platform_toolset_known() {
        assert_eq!(platform_toolset("cli", ""), "hermez-cli");
        assert_eq!(platform_toolset("telegram", ""), "hermez-telegram");
    }

    #[test]
    fn test_platform_info() {
        let info = platform_info("slack").unwrap();
        assert_eq!(info.label, "💼 Slack");
        assert_eq!(info.default_toolset, "hermez-slack");
    }

    #[test]
    fn test_is_known_platform() {
        assert!(is_known_platform("whatsapp"));
        assert!(!is_known_platform("not_a_platform"));
    }

    #[test]
    fn test_platform_keys_count() {
        let keys: Vec<_> = platform_keys().collect();
        assert_eq!(keys.len(), PLATFORMS.len());
        assert!(keys.contains(&"cli"));
        assert!(keys.contains(&"webhook"));
    }
}
