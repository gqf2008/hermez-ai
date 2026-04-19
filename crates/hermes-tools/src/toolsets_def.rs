//! Toolset definitions.
//!
//! Mirrors the Python `toolsets.py` TOOLSETS dict.
//! Toolsets group tools by capability and support composition via `includes`.

use std::collections::{HashMap, HashSet};

/// A toolset definition: a named group of tools with optional includes.
#[derive(Debug, Clone)]
pub struct ToolsetDef {
    /// Human-readable description.
    pub description: &'static str,
    /// Direct tool names in this toolset.
    pub tools: &'static [&'static str],
    /// Other toolsets this one includes (composition).
    pub includes: &'static [&'static str],
}

/// The master list of Hermes core tools shared across all messaging platforms.
///
/// Mirrors the Python `_HERMES_CORE_TOOLS` list.
pub const HERMES_CORE_TOOLS: &[&str] = &[
    // Web
    "web_search",
    "web_extract",
    // Terminal + process management
    "terminal",
    "process",
    // File manipulation
    "read_file",
    "write_file",
    "patch",
    "search_files",
    // Vision + image generation
    "vision_analyze",
    "image_generate",
    // Skills
    "skills_list",
    "skill_view",
    "skill_manage",
    // Browser automation
    "browser_navigate",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_scroll",
    "browser_back",
    "browser_press",
    "browser_get_images",
    "browser_vision",
    "browser_console",
    // Text-to-speech
    "text_to_speech",
    // Planning & memory
    "todo",
    "memory",
    // Session history search
    "session_search",
    // Clarifying questions
    "clarify",
    // Code execution + delegation
    "execute_code",
    "delegate_task",
    // Cronjob management
    "cronjob",
    // Cross-platform messaging
    "send_message",
    // Home Assistant smart home
    "ha_list_entities",
    "ha_get_state",
    "ha_list_services",
    "ha_call_service",
];

/// All defined toolsets.
///
/// Mirrors the Python `TOOLSETS` dict.
pub fn toolsets() -> HashMap<&'static str, ToolsetDef> {
    let mut map = HashMap::new();

    // Leaf toolsets
    map.insert("web", ToolsetDef {
        description: "Web search and extraction tools",
        tools: &["web_search", "web_extract"],
        includes: &[],
    });

    map.insert("search", ToolsetDef {
        description: "Web search only (no content extraction/scraping)",
        tools: &["web_search"],
        includes: &[],
    });

    map.insert("vision", ToolsetDef {
        description: "Image analysis and vision tools",
        tools: &["vision_analyze"],
        includes: &[],
    });

    map.insert("image_gen", ToolsetDef {
        description: "Creative generation tools (images)",
        tools: &["image_generate"],
        includes: &[],
    });

    map.insert("terminal", ToolsetDef {
        description: "Terminal/command execution and process management tools",
        tools: &["terminal", "process"],
        includes: &[],
    });

    map.insert("file", ToolsetDef {
        description: "File read, write, search, and patch tools",
        tools: &["read_file", "write_file", "search_files", "patch"],
        includes: &[],
    });

    map.insert("browser", ToolsetDef {
        description: "Browser automation for web interaction with web search for finding URLs",
        tools: &[
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_scroll",
            "browser_back",
            "browser_press",
            "browser_get_images",
            "browser_vision",
            "browser_console",
            "web_search",
        ],
        includes: &[],
    });

    map.insert("skills", ToolsetDef {
        description: "Skill management tools",
        tools: &["skills_list", "skill_view", "skill_manage"],
        includes: &[],
    });

    map.insert("cronjob", ToolsetDef {
        description: "Cronjob management tool",
        tools: &["cronjob"],
        includes: &[],
    });

    map.insert("messaging", ToolsetDef {
        description: "Cross-platform messaging",
        tools: &["send_message"],
        includes: &[],
    });

    map.insert("rl", ToolsetDef {
        description: "RL training tools for reinforcement learning",
        tools: &["rl_training"],
        includes: &[],
    });

    map.insert("tts", ToolsetDef {
        description: "Text-to-speech conversion",
        tools: &["text_to_speech"],
        includes: &[],
    });

    map.insert("todo", ToolsetDef {
        description: "Task planning and tracking",
        tools: &["todo"],
        includes: &[],
    });

    map.insert("memory", ToolsetDef {
        description: "Persistent memory across sessions",
        tools: &["memory"],
        includes: &[],
    });

    map.insert("session_search", ToolsetDef {
        description: "Search and recall past conversations",
        tools: &["session_search"],
        includes: &[],
    });

    map.insert("clarify", ToolsetDef {
        description: "Ask clarifying questions",
        tools: &["clarify"],
        includes: &[],
    });

    map.insert("code_execution", ToolsetDef {
        description: "Programmatic code execution sandbox",
        tools: &["execute_code"],
        includes: &[],
    });

    map.insert("delegation", ToolsetDef {
        description: "Sub-agent delegation tools",
        tools: &["delegate_task"],
        includes: &[],
    });

    map.insert("homeassistant", ToolsetDef {
        description: "Home Assistant smart home control tools",
        tools: &["ha_list_entities", "ha_get_state", "ha_list_services", "ha_call_service"],
        includes: &[],
    });

    map.insert("moa", ToolsetDef {
        description: "Mixture-of-Agents collaborative reasoning",
        tools: &["mixture_of_agents"],
        includes: &[],
    });

    map.insert("organization", ToolsetDef {
        description: "Task organization tools",
        tools: &["todo", "session_search", "clarify", "memory"],
        includes: &[],
    });

    // Scenario toolsets
    map.insert("debugging", ToolsetDef {
        description: "Tools for debugging (web + file)",
        tools: &[],
        includes: &["web", "file"],
    });

    map.insert("safe", ToolsetDef {
        description: "Safe tools only (no terminal execution)",
        tools: &[],
        includes: &["web", "file", "browser", "skills", "vision", "organization"],
    });

    // Platform toolsets — all reference HERMES_CORE_TOOLS.
    // Mirrors Python toolsets.py:68-396.

    map.insert("hermes-acp", ToolsetDef {
        description: "Editor integration (VS Code, Zed, JetBrains) — coding-focused tools without messaging, audio, or clarify UI",
        tools: &[
            "web_search", "web_extract",
            "terminal", "process",
            "read_file", "write_file", "patch", "search_files",
            "vision_analyze",
            "skills_list", "skill_view", "skill_manage",
            "browser_navigate", "browser_snapshot", "browser_click",
            "browser_type", "browser_scroll", "browser_back",
            "browser_press", "browser_get_images",
            "browser_vision", "browser_console",
            "todo", "memory",
            "session_search",
            "execute_code", "delegate_task",
        ],
        includes: &[],
    });

    map.insert("hermes-api-server", ToolsetDef {
        description: "OpenAI-compatible API server — full agent tools accessible via HTTP (no interactive UI tools like clarify or send_message)",
        tools: &[
            // Web
            "web_search", "web_extract",
            // Terminal + process management
            "terminal", "process",
            // File manipulation
            "read_file", "write_file", "patch", "search_files",
            // Vision + image generation
            "vision_analyze", "image_generate",
            // Skills
            "skills_list", "skill_view", "skill_manage",
            // Browser automation
            "browser_navigate", "browser_snapshot", "browser_click",
            "browser_type", "browser_scroll", "browser_back",
            "browser_press", "browser_get_images",
            "browser_vision", "browser_console",
            // Planning & memory
            "todo", "memory",
            // Session history search
            "session_search",
            // Code execution + delegation
            "execute_code", "delegate_task",
            // Cronjob management
            "cronjob",
            // Home Assistant smart home control
            "ha_list_entities", "ha_get_state", "ha_list_services", "ha_call_service",
        ],
        includes: &[],
    });

    map.insert("hermes-cli", ToolsetDef {
        description: "CLI platform — all core tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-telegram", ToolsetDef {
        description: "Telegram platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-discord", ToolsetDef {
        description: "Discord platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-whatsapp", ToolsetDef {
        description: "WhatsApp platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-slack", ToolsetDef {
        description: "Slack platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-signal", ToolsetDef {
        description: "Signal platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-bluebubbles", ToolsetDef {
        description: "BlueBubbles (iMessage) platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-homeassistant", ToolsetDef {
        description: "Home Assistant platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-email", ToolsetDef {
        description: "Email platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-sms", ToolsetDef {
        description: "SMS platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-mattermost", ToolsetDef {
        description: "Mattermost platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-matrix", ToolsetDef {
        description: "Matrix platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-dingtalk", ToolsetDef {
        description: "DingTalk platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-feishu", ToolsetDef {
        description: "Feishu platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-weixin", ToolsetDef {
        description: "Weixin (WeChat) platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-qqbot", ToolsetDef {
        description: "QQ Bot platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-wecom", ToolsetDef {
        description: "WeCom platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-wecom-callback", ToolsetDef {
        description: "WeCom callback platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    map.insert("hermes-webhook", ToolsetDef {
        description: "Generic webhook platform tools",
        tools: HERMES_CORE_TOOLS,
        includes: &[],
    });

    // Gateway toolset: union of all platforms
    map.insert("hermes-gateway", ToolsetDef {
        description: "Gateway — union of all platform toolsets",
        tools: &[],
        includes: &[
            "hermes-telegram", "hermes-discord", "hermes-whatsapp",
            "hermes-slack", "hermes-signal", "hermes-bluebubbles",
            "hermes-homeassistant", "hermes-email", "hermes-sms",
            "hermes-mattermost", "hermes-matrix", "hermes-dingtalk",
            "hermes-feishu", "hermes-weixin", "hermes-qqbot",
            "hermes-wecom", "hermes-wecom-callback", "hermes-webhook",
        ],
    });

    map
}

/// Resolve a toolset name to a flat list of tool names.
///
/// Recursively expands `includes` and deduplicates.
/// Returns None if the toolset name is not found.
pub fn resolve_toolset(name: &str) -> Option<Vec<String>> {
    let all_toolsets = toolsets();

    // Check for aliases
    let actual_name = match name {
        "all" | "*" => return Some(HERMES_CORE_TOOLS.iter().map(|s| s.to_string()).collect()),
        _ => name,
    };

    let ts = all_toolsets.get(actual_name)?;

    let mut result: HashSet<String> = ts.tools.iter().map(|s| s.to_string()).collect();

    // Recursively resolve includes
    for included in ts.includes {
        if let Some(included_tools) = resolve_toolset(included) {
            result.extend(included_tools);
        }
    }

    Some(result.into_iter().collect())
}

/// Validate that a toolset name exists.
pub fn validate_toolset(name: &str) -> bool {
    matches!(name, "all" | "*") || toolsets().contains_key(name)
}

/// Create a custom toolset at runtime.
///
/// Returns a `ToolsetDef` with the given tools. Note: this leaks memory for
/// the lifetime of the process. This is acceptable since custom toolsets
/// are created rarely (if ever) and are not called in the hot path.
pub fn create_custom_toolset(
    _name: String,
    tools: Vec<String>,
    description: String,
) -> ToolsetDef {
    let boxed_tools: Vec<&'static str> = tools
        .into_iter()
        .map(|s| Box::leak(s.into_boxed_str()) as &'static str)
        .collect();
    ToolsetDef {
        description: Box::leak(description.into_boxed_str()),
        tools: Box::leak(boxed_tools.into_boxed_slice()),
        includes: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_leaf_toolset() {
        let tools = resolve_toolset("web").unwrap();
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.contains(&"web_extract".to_string()));
    }

    #[test]
    fn test_resolve_composed_toolset() {
        let tools = resolve_toolset("debugging").unwrap();
        // debugging includes web + file
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_resolve_all_alias() {
        let tools = resolve_toolset("all").unwrap();
        assert!(!tools.is_empty());
    }

    #[test]
    fn test_validate_toolset() {
        assert!(validate_toolset("web"));
        assert!(validate_toolset("all"));
        assert!(validate_toolset("*"));
        assert!(!validate_toolset("nonexistent"));
    }

    #[test]
    fn test_platform_toolsets_exist() {
        let platforms = [
            "hermes-cli", "hermes-acp", "hermes-api-server",
            "hermes-telegram", "hermes-discord",
            "hermes-whatsapp", "hermes-slack", "hermes-signal",
            "hermes-bluebubbles", "hermes-homeassistant", "hermes-email",
            "hermes-sms", "hermes-mattermost", "hermes-matrix",
            "hermes-dingtalk", "hermes-feishu", "hermes-weixin",
            "hermes-qqbot", "hermes-wecom", "hermes-wecom-callback",
            "hermes-webhook", "hermes-gateway",
        ];
        let ts = toolsets();
        for platform in platforms {
            assert!(ts.contains_key(platform), "Missing platform toolset: {}", platform);
        }
    }

    #[test]
    fn test_platform_toolset_has_core_tools() {
        let tools = resolve_toolset("hermes-telegram").unwrap();
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.contains(&"terminal".to_string()));
        assert!(tools.contains(&"send_message".to_string()));
    }

    #[test]
    fn test_gateway_includes_all_platforms() {
        let tools = resolve_toolset("hermes-gateway").unwrap();
        // Gateway should include all core tools via platform includes
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.contains(&"browser_navigate".to_string()));
        assert!(tools.contains(&"send_message".to_string()));
    }
}
