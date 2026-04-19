#![allow(dead_code)]
//! MCP (Model Context Protocol) server management.

use console::Style;
use std::path::PathBuf;

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

fn green() -> Style { Style::new().green() }
fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }
fn red() -> Style { Style::new().red() }

fn mcp_config_path() -> PathBuf {
    get_hermes_home().join("mcp_servers.json")
}

/// MCP server configuration.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct MCPServer {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preset: Option<String>,
    #[serde(default)]
    env: Vec<String>,
}

fn load_mcp_servers() -> Vec<MCPServer> {
    let path = mcp_config_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(servers) = serde_json::from_str::<Vec<MCPServer>>(&content) {
                return servers;
            }
        }
    }
    Vec::new()
}

fn save_mcp_servers(servers: &[MCPServer]) -> anyhow::Result<()> {
    let path = mcp_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(servers)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// List MCP servers.
pub fn cmd_mcp_list() -> anyhow::Result<()> {
    let servers = load_mcp_servers();

    println!();
    println!("{}", cyan().apply_to("◆ MCP Servers"));
    println!();

    if servers.is_empty() {
        println!("  {}", dim().apply_to("No MCP servers configured."));
        println!("  Add one with: hermes mcp add <name> --command <cmd>");
    } else {
        for server in &servers {
            let status = if server.enabled {
                green().apply_to("enabled").to_string()
            } else {
                yellow().apply_to("disabled").to_string()
            };
            print!("  {} — {}", server.name, status);
            if let Some(ref u) = server.url {
                print!(" (url: {u})");
            }
            if let Some(ref c) = server.command {
                print!(" ({c}",);
                if !server.args.is_empty() {
                    print!(" {}", server.args.join(" "));
                }
                print!(")");
            }
            println!();
            if let Some(ref a) = server.auth {
                println!("    auth: {a}");
            }
            if let Some(ref p) = server.preset {
                println!("    preset: {p}");
            }
            if !server.env.is_empty() {
                println!("    env: {}", server.env.join(" "));
            }
        }
    }
    println!();

    Ok(())
}

/// Known MCP presets.
fn apply_preset(preset: &str) -> Option<(String, Vec<String>)> {
    match preset {
        "wassette" => Some(("wassette".to_string(), vec![])),
        "playwright" => Some(("npx".to_string(), vec!["-y".to_string(), "@anthropic-ai/playwright-mcp".to_string()])),
        "git" => Some(("uvx".to_string(), vec!["mcp-server-git".to_string(), "--repository".to_string(), ".".to_string()])),
        _ => None,
    }
}

/// Add an MCP server.
pub fn cmd_mcp_add(
    name: &str,
    url: Option<&str>,
    command: Option<&str>,
    args: &[String],
    auth: Option<&str>,
    preset: Option<&str>,
    env: &[String],
) -> anyhow::Result<()> {
    let mut servers = load_mcp_servers();

    // Check for duplicate
    if servers.iter().any(|s| s.name == name) {
        println!("  {} MCP server already exists: {}", yellow().apply_to("⚠"), name);
        return Ok(());
    }

    // Apply preset if specified
    let (final_command, final_args, final_preset) = if let Some(p) = preset {
        if let Some((cmd, preset_args)) = apply_preset(p) {
            (Some(cmd), preset_args, Some(p.to_string()))
        } else {
            println!("  {} Unknown preset: {}. Known presets: wassette, playwright, git", yellow().apply_to("⚠"), p);
            return Ok(());
        }
    } else {
        (command.map(String::from), args.to_vec(), preset.map(String::from))
    };

    // Require either url or command
    if url.is_none() && final_command.is_none() {
        println!("  {} Specify either --url, --command, or --preset.", yellow().apply_to("⚠"));
        return Ok(());
    }

    servers.push(MCPServer {
        name: name.to_string(),
        url: url.map(String::from),
        command: final_command,
        args: final_args,
        enabled: true,
        auth: auth.map(String::from),
        preset: final_preset,
        env: env.to_vec(),
    });

    save_mcp_servers(&servers)?;
    println!("  {} MCP server added: {}", green().apply_to("✓"), name);
    if let Some(u) = url {
        println!("    URL: {u}");
    }
    if let Some(ref c) = servers.last().unwrap().command {
        println!("    Command: {c} {}", servers.last().unwrap().args.join(" "));
    }
    if let Some(a) = auth {
        println!("    Auth: {a}");
    }
    if let Some(ref p) = servers.last().unwrap().preset {
        println!("    Preset: {p}");
    }
    if !env.is_empty() {
        println!("    Env: {}", env.join(" "));
    }

    Ok(())
}

/// Remove an MCP server.
pub fn cmd_mcp_remove(name: &str) -> anyhow::Result<()> {
    let mut servers = load_mcp_servers();
    let before = servers.len();
    servers.retain(|s| s.name != name);

    if servers.len() < before {
        save_mcp_servers(&servers)?;
        println!("  {} MCP server removed: {}", green().apply_to("✓"), name);
    } else {
        println!("  {} MCP server not found: {}", yellow().apply_to("✗"), name);
    }

    Ok(())
}

/// Test connection to an MCP server.
pub fn cmd_mcp_test(name: &str) -> anyhow::Result<()> {
    let servers = load_mcp_servers();
    let server = servers.iter().find(|s| s.name == name);

    match server {
        Some(s) => {
            println!("  Testing MCP server: {}", s.name);
            match (&s.url, &s.command) {
                (Some(u), _) => println!("  URL: {u}"),
                (_, Some(c)) => println!("  Command: {c} {}", s.args.join(" ")),
                (None, None) => println!("  {}", yellow().apply_to("⚠ No URL or command configured.")),
            }
            println!();

            // Try to run the command with --help or version
            if let Some(ref cmd) = s.command {
                let output = std::process::Command::new(cmd)
                    .args(&s.args)
                    .arg("--version")
                    .output();

                match output {
                    Ok(out) if out.status.success() => {
                        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        println!("  {} Connected — version: {}", green().apply_to("✓"), version);
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        println!("  {} Connection failed: {}", yellow().apply_to("⚠"), stderr.trim());
                    }
                    Err(e) => {
                        println!("  {} Failed to execute: {}", red().apply_to("✗"), e);
                    }
                }
            } else {
                println!("  {}", dim().apply_to("URL-based server — use curl or http client to test connectivity."));
            }
        }
        None => {
            println!("  {} MCP server not found: {}", yellow().apply_to("✗"), name);
        }
    }

    println!();
    Ok(())
}

/// Configure MCP servers interactively.
pub fn cmd_mcp_configure(_name: &str) -> anyhow::Result<()> {
    let servers = load_mcp_servers();

    println!();
    println!("{}", cyan().apply_to("◆ MCP Server Configuration"));
    println!();

    if servers.is_empty() {
        println!("  No MCP servers configured.");
    } else {
        for (i, server) in servers.iter().enumerate() {
            let status = if server.enabled { "ON" } else { "OFF" };
            let location = match (&server.url, &server.command) {
                (Some(u), _) => format!("url: {u}"),
                (_, Some(c)) => format!("{c} {}", server.args.join(" ")),
                (None, None) => "(none)".to_string(),
            };
            println!("  {}. {} [{status}] — {location}", i + 1, server.name);
        }
    }
    println!();
    println!("  Add with: hermes mcp add <name> --command <cmd> [--args \"...\"]");
    println!();

    Ok(())
}

/// Dispatch MCP subcommands.
pub fn cmd_mcp(
    action: &str,
    name: Option<&str>,
    command: &str,
    args: &[String],
) -> anyhow::Result<()> {
    match action {
        "list" | "ls" | "" => cmd_mcp_list(),
        "add" | "register" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_mcp_add(n, None, if command.is_empty() { None } else { Some(command) }, args, None, None, &[])
        }
        "remove" | "rm" | "unregister" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_mcp_remove(n)
        }
        "test" | "ping" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_mcp_test(n)
        }
        "configure" | "config" => cmd_mcp_configure(""),
        _ => {
            anyhow::bail!("Unknown action: {}. Use list, add, remove, test, or configure.", action);
        }
    }
}

/// Run as MCP stdio server.
pub fn cmd_mcp_serve(verbose: bool) -> anyhow::Result<()> {
    if !verbose {
        println!();
        println!("{}", cyan().apply_to("◆ MCP Stdio Server"));
        println!();
        println!("  {}", dim().apply_to("Starting Hermes MCP server over stdio..."));
        println!();
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(hermes_tools::mcp_serve::run_mcp_server(verbose))
        .map_err(|e| anyhow::anyhow!("MCP server failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_server_deserialize_legacy_format() {
        // Legacy config had command as required String; new format makes it Option<String>
        let legacy_json = r#"{"name":"test","command":"python","args":["-m","server"],"enabled":true}"#;
        let server: MCPServer = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(server.name, "test");
        assert_eq!(server.command, Some("python".to_string()));
        assert!(server.url.is_none());
        assert!(server.auth.is_none());
        assert!(server.preset.is_none());
        assert_eq!(server.args, vec!["-m", "server"]);
        assert!(server.enabled);
    }

    #[test]
    fn test_mcp_server_deserialize_new_format() {
        let new_json = r#"{"name":"remote","url":"http://localhost:8080","auth":"bearer","preset":"github","env":["FOO=bar"]}"#;
        let server: MCPServer = serde_json::from_str(new_json).unwrap();
        assert_eq!(server.name, "remote");
        assert_eq!(server.url, Some("http://localhost:8080".to_string()));
        assert!(server.command.is_none());
        assert_eq!(server.auth, Some("bearer".to_string()));
        assert_eq!(server.preset, Some("github".to_string()));
        assert_eq!(server.env, vec!["FOO=bar"]);
    }

    #[test]
    fn test_mcp_server_deserialize_empty_defaults() {
        let minimal = r#"{"name":"minimal"}"#;
        let server: MCPServer = serde_json::from_str(minimal).unwrap();
        assert_eq!(server.name, "minimal");
        assert!(server.command.is_none());
        assert!(server.url.is_none());
        assert!(server.args.is_empty());
        assert!(!server.enabled);
        assert!(server.auth.is_none());
        assert!(server.preset.is_none());
        assert!(server.env.is_empty());
    }
}
