#![allow(dead_code)]
//! MCP Server Management.
//!
//! Mirrors the Python `hermez_cli/mcp_config.py`.
//! Provides functions for adding, removing, listing, and testing MCP servers
//! stored in `~/.hermez/config.yaml` under the `mcp_servers` key.

use std::collections::BTreeMap;

use hermez_core::config::McpServerConfig;

/// MCP server management operations.
pub struct McpManager;

impl McpManager {
    /// Get the `mcp_servers` dict from config, or empty map.
    pub fn get_mcp_servers() -> BTreeMap<String, McpServerConfig> {
        let Ok(config) = hermez_core::HermezConfig::load() else {
            return BTreeMap::new();
        };
        // hermez_core uses HashMap, we sort it for display
        config.mcp_servers.into_iter().collect()
    }

    /// Add or update a server entry in config.yaml.
    pub fn save_mcp_server(name: &str, server_config: &McpServerConfig) -> Result<(), String> {
        let mut config = hermez_core::HermezConfig::load()
            .map_err(|e| format!("Failed to load config: {e}"))?;
        config.mcp_servers.insert(name.to_string(), server_config.clone());
        config.save().map_err(|e| format!("Failed to save config: {e}"))
    }

    /// Remove a server from config.yaml. Returns true if it existed.
    pub fn remove_mcp_server(name: &str) -> Result<bool, String> {
        let mut config = hermez_core::HermezConfig::load()
            .map_err(|e| format!("Failed to load config: {e}"))?;
        let existed = config.mcp_servers.remove(name).is_some();
        if existed {
            config.save().map_err(|e| format!("Failed to save config: {e}"))?;
        }
        Ok(existed)
    }

    /// List all configured MCP servers as a formatted string.
    pub fn list_servers() -> String {
        let servers = Self::get_mcp_servers();
        if servers.is_empty() {
            return "No MCP servers configured.\nAdd one with: hermez mcp add <name> --url <endpoint>\n".to_string();
        }

        let mut lines = vec!["MCP Servers:".to_string()];
        lines.push(format!("  {:<16} {:<30} {:<12} {}", "Name", "Transport", "Tools", "Status"));
        lines.push(format!("  {:─<16} {:─<30} {:─<12} {:─<10}", "", "", "", ""));

        for (name, cfg) in &servers {
            let transport = if let Some(url) = &cfg.url {
                if url.len() > 28 {
                    format!("{}...", &url[..25])
                } else {
                    url.clone()
                }
            } else if let Some(cmd) = &cfg.command {
                let args = cfg.args.as_ref().map(|a| a.iter().take(2).cloned().collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let transport = if args.is_empty() {
                    cmd.clone()
                } else {
                    format!("{cmd} {args}")
                };
                if transport.len() > 28 {
                    format!("{}...", &transport[..25])
                } else {
                    transport
                }
            } else {
                "?".to_string()
            };

            let tools_str = "all".to_string();
            let status = "enabled";

            lines.push(format!("  {name:<16} {transport:<30} {tools_str:<12} {status}"));
        }

        lines.join("\n")
    }

    /// Build the env-var key for a server's API key.
    pub fn env_key_for_server(name: &str) -> String {
        format!("MCP_{}_API_KEY", name.replace('-', "_").to_uppercase())
    }

    /// Parse KEY=VALUE strings from CLI args into an env dict.
    pub fn parse_env_assignments(raw_env: &[&str]) -> Result<BTreeMap<String, String>, String> {
        let name_re = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap();
        let mut parsed = BTreeMap::new();
        for item in raw_env {
            let text = item.trim();
            if text.is_empty() {
                continue;
            }
            let Some(eq_idx) = text.find('=') else {
                return Err(format!("Invalid --env value '{text}' (expected KEY=VALUE)"));
            };
            let key = text[..eq_idx].trim();
            if key.is_empty() {
                return Err(format!("Invalid --env value '{text}' (missing variable name)"));
            }
            if !name_re.is_match(key) {
                return Err(format!("Invalid --env variable name '{key}'"));
            }
            parsed.insert(key.to_string(), text[eq_idx + 1..].to_string());
        }
        Ok(parsed)
    }

    /// Resolve `${ENV_VAR}` references in a string.
    pub fn interpolate_value(value: &str) -> String {
        let re = regex::Regex::new(r"\$\{(\w+)\}").unwrap();
        re.replace_all(value, |caps: &regex::Captures| {
            std::env::var(caps.get(1).unwrap().as_str()).unwrap_or_default()
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_key_for_server() {
        assert_eq!(McpManager::env_key_for_server("my-server"), "MCP_MY_SERVER_API_KEY");
        assert_eq!(McpManager::env_key_for_server("test"), "MCP_TEST_API_KEY");
    }

    #[test]
    fn test_parse_env_assignments() {
        let result = McpManager::parse_env_assignments(&["KEY=value", "FOO=bar=baz"]).unwrap();
        assert_eq!(result.get("KEY"), Some(&"value".to_string()));
        assert_eq!(result.get("FOO"), Some(&"bar=baz".to_string()));
    }

    #[test]
    fn test_parse_env_assignments_invalid() {
        assert!(McpManager::parse_env_assignments(&["no_equals"]).is_err());
        assert!(McpManager::parse_env_assignments(&["=value"]).is_err());
        assert!(McpManager::parse_env_assignments(&["1INVALID=value"]).is_err());
    }

    #[test]
    fn test_parse_env_assignments_empty() {
        let result = McpManager::parse_env_assignments(&["", "  ", "KEY=val"]).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_interpolate_value() {
        std::env::set_var("HERMEZ_TEST_MCP_VAR", "secret123");
        let result = McpManager::interpolate_value("${HERMEZ_TEST_MCP_VAR}");
        assert_eq!(result, "secret123");
        let result = McpManager::interpolate_value("Bearer ${HERMEZ_TEST_MCP_VAR}");
        assert_eq!(result, "Bearer secret123");
        let result = McpManager::interpolate_value("no vars here");
        assert_eq!(result, "no vars here");
    }

    #[test]
    fn test_list_servers_empty() {
        let output = McpManager::list_servers();
        assert!(output.contains("No MCP servers configured"));
    }
}
